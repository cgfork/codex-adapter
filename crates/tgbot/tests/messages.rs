use codebridge_core::{
    ApprovalDecision, Command as CoreCommand, IncomingMessage as CoreIncomingMessage,
};
use serde_json::{Value, json};
use teloxide::{
    types::{CallbackQuery, Message},
    utils::command::BotCommands,
};
use tgbot::{ApprovalSignal, Command, IncomingMessage};

fn message_payload() -> Value {
    json!({
        "message_id": 42,
        "date": 1_700_000_000,
        "chat": { "id": 123, "type": "private", "first_name": "Alice" },
        "from": { "id": 123, "is_bot": false, "first_name": "Alice" }
    })
}

fn text_message(text: &str) -> Message {
    let mut payload = message_payload();
    payload["text"] = json!(text);
    serde_json::from_value(payload).unwrap()
}

fn callback_query(data: Option<&str>) -> CallbackQuery {
    let mut message = message_payload();
    message["text"] = json!("Approve this request?");
    serde_json::from_value(json!({
        "id": "approval-query",
        "from": { "id": 123, "is_bot": false, "first_name": "Alice" },
        "chat_instance": "chat-instance",
        "message": message,
        "data": data
    }))
    .unwrap()
}

#[test]
fn recognizes_all_commands_and_preserves_the_message() {
    for (text, expected) in [
        ("/sessions", Command::Sessions),
        ("/interrupt", Command::Interrupt),
        ("/init", Command::Init),
        ("/new", Command::New),
        ("/delete", Command::Delete),
    ] {
        let original = text_message(text);
        let IncomingMessage::Command { command, message } =
            IncomingMessage::from_message(original.clone(), "codebridge_bot")
        else {
            panic!("expected command for {text}");
        };

        assert_eq!(command, expected);
        assert_eq!(message.id, original.id);
        assert_eq!(message.chat.id, original.chat.id);
        assert_eq!(message.text(), Some(text));
    }
}

#[test]
fn recognizes_commands_addressed_to_this_bot() {
    for (text, expected) in [
        ("/sessions@codebridge_bot", Command::Sessions),
        ("/delete@codebridge_bot", Command::Delete),
    ] {
        let IncomingMessage::Command { command, .. } =
            IncomingMessage::from_message(text_message(text), "codebridge_bot")
        else {
            panic!("expected command for {text}");
        };

        assert_eq!(command, expected);
    }
}

#[test]
fn registers_the_delete_command() {
    assert!(Command::bot_commands().iter().any(|command| {
        command.command == "/delete" && command.description == "Delete the current session"
    }));
}

#[test]
fn preserves_plain_text_unknown_commands_and_commands_for_other_bots_as_raw_messages() {
    for text in [
        "hello",
        "allow",
        "deny",
        "/unknown",
        "/sessions@another_bot",
        "/delete@another_bot",
        "please /sessions",
    ] {
        let IncomingMessage::Raw(message) =
            IncomingMessage::from_message(text_message(text), "codebridge_bot")
        else {
            panic!("expected raw message for {text}");
        };

        assert_eq!(message.text(), Some(text));
    }
}

#[test]
fn preserves_media_as_raw_messages() {
    let mut payload = message_payload();
    payload["photo"] = json!([{
        "file_id": "photo-id",
        "file_unique_id": "unique-photo-id",
        "width": 640,
        "height": 480
    }]);
    let message = serde_json::from_value(payload).unwrap();
    let IncomingMessage::Raw(message) = IncomingMessage::from_message(message, "codebridge_bot")
    else {
        panic!("expected raw media message");
    };

    assert!(message.photo().is_some());
    assert_eq!(message.chat.id.0, 123);
}

#[test]
fn recognizes_approval_signals_and_preserves_the_callback() {
    for signal in [ApprovalSignal::Allow, ApprovalSignal::Deny] {
        let original = callback_query(Some(signal.callback_data()));
        let Some(IncomingMessage::ApprovalSignal {
            signal: received,
            query,
        }) = IncomingMessage::from_callback_query(original.clone())
        else {
            panic!("expected approval signal for {signal:?}");
        };

        assert_eq!(received, signal);
        assert_eq!(query.id, original.id);
        assert_eq!(query.from.id, original.from.id);
        assert_eq!(query.data.as_deref(), Some(signal.callback_data()));
        assert!(query.message.is_some());
    }
}

#[test]
fn ignores_callbacks_without_recognized_approval_data() {
    for data in [None, Some(""), Some("unknown"), Some("allow:unknown")] {
        assert!(IncomingMessage::from_callback_query(callback_query(data)).is_none());
    }
}

#[test]
fn normalizes_all_commands_and_plain_text() {
    for (text, expected) in [
        ("/sessions", Some(CoreCommand::Sessions)),
        ("/interrupt", Some(CoreCommand::Interrupt)),
        ("/init", Some(CoreCommand::Init)),
        ("/new", Some(CoreCommand::New)),
        ("/delete", Some(CoreCommand::Delete)),
        ("/exec task", Some(CoreCommand::Exec("task".into()))),
        ("hello", None),
        ("/sessions@other_bot", None),
    ] {
        let event = IncomingMessage::from_message(text_message(text), "codebridge_bot")
            .into_core()
            .unwrap();
        let CoreIncomingMessage::Message {
            chat_id,
            sender,
            command,
            text: body,
        } = event
        else {
            panic!("expected message");
        };
        assert_eq!(chat_id.0, 123);
        assert_eq!(sender.unwrap().0, 123);
        assert_eq!(command, expected);
        assert_eq!(body.as_deref(), Some(text));
    }
}

#[test]
fn normalized_sender_only_identifies_a_person_acting_as_themselves() {
    let mut payload = message_payload();
    payload["text"] = json!("/delete");
    for change in ["missing", "bot", "anonymous", "channel"] {
        let mut case = payload.clone();
        match change {
            "missing" => {
                case.as_object_mut().unwrap().remove("from");
            }
            "bot" => case["from"]["is_bot"] = json!(true),
            "anonymous" => {
                case["sender_chat"] =
                    json!({"id": -100123, "type": "supergroup", "title": "Admins"})
            }
            "channel" => {
                case["sender_chat"] = json!({"id": -100456, "type": "channel", "title": "News"})
            }
            _ => unreachable!(),
        }
        let message: Message = serde_json::from_value(case).unwrap();
        let CoreIncomingMessage::Message {
            sender, command, ..
        } = IncomingMessage::from_message(message, "codebridge_bot")
            .into_core()
            .unwrap()
        else {
            panic!("expected message")
        };
        assert_eq!(sender, None, "{change}");
        assert_eq!(command, Some(CoreCommand::Delete));
    }

    let mut forwarded = payload;
    forwarded["from"]["id"] = json!(7);
    forwarded["forward_origin"] = json!({"type":"user","date":1_700_000_000,
        "sender_user":{"id":123,"is_bot":false,"first_name":"Owner"}});
    let CoreIncomingMessage::Message { sender, .. } =
        IncomingMessage::from_message(serde_json::from_value(forwarded).unwrap(), "codebridge_bot")
            .into_core()
            .unwrap()
    else {
        panic!("expected message")
    };
    assert_eq!(sender.unwrap().0, 7);
}

#[test]
fn media_and_approval_callbacks_normalize_without_inventing_identity() {
    let mut photo = message_payload();
    photo["photo"] =
        json!([{"file_id":"photo-id","file_unique_id":"unique-id","width":1,"height":1}]);
    let CoreIncomingMessage::Message { text, sender, .. } =
        IncomingMessage::from_message(serde_json::from_value(photo).unwrap(), "codebridge_bot")
            .into_core()
            .unwrap()
    else {
        panic!("expected message")
    };
    assert_eq!(text, None);
    assert_eq!(sender.unwrap().0, 123);

    for (signal, decision) in [
        (ApprovalSignal::Allow, ApprovalDecision::Allow),
        (ApprovalSignal::Deny, ApprovalDecision::Deny),
    ] {
        let event =
            IncomingMessage::from_callback_query(callback_query(Some(signal.callback_data())))
                .unwrap()
                .into_core()
                .unwrap();
        assert_eq!(
            event,
            CoreIncomingMessage::Approval {
                chat_id: teloxide::types::ChatId(123),
                message_id: teloxide::types::MessageId(42),
                sender: Some(teloxide::types::UserId(123)),
                decision,
            }
        );
    }

    let mut bot_callback = callback_query(Some("allow"));
    bot_callback.from.is_bot = true;
    let CoreIncomingMessage::Approval { sender, .. } =
        IncomingMessage::from_callback_query(bot_callback)
            .unwrap()
            .into_core()
            .unwrap()
    else {
        panic!("expected approval")
    };
    assert_eq!(sender, None);

    let mut inline = callback_query(Some("allow"));
    inline.message = None;
    assert!(
        IncomingMessage::from_callback_query(inline)
            .unwrap()
            .into_core()
            .is_none()
    );
}
