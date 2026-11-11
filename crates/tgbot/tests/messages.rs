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
            IncomingMessage::from_message(original.clone(), "adapter_bot")
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
        ("/sessions@adapter_bot", Command::Sessions),
        ("/delete@adapter_bot", Command::Delete),
    ] {
        let IncomingMessage::Command { command, .. } =
            IncomingMessage::from_message(text_message(text), "adapter_bot")
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
            IncomingMessage::from_message(text_message(text), "adapter_bot")
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
    let IncomingMessage::Raw(message) = IncomingMessage::from_message(message, "adapter_bot")
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
