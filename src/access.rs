use teloxide::types::UserId;
use tgbot::IncomingMessage;

pub(crate) fn is_allowed(allowed_user_id: Option<UserId>, incoming: &IncomingMessage) -> bool {
    let Some(allowed_user_id) = allowed_user_id else {
        return true;
    };
    match incoming {
        IncomingMessage::Raw(message) | IncomingMessage::Command { message, .. } => {
            // Messages sent on behalf of a chat do not identify a personal account.
            message.sender_chat.is_none()
                && message
                    .from
                    .as_ref()
                    .is_some_and(|user| user.id == allowed_user_id && !user.is_bot)
        }
        IncomingMessage::ApprovalSignal { query, .. } => {
            query.from.id == allowed_user_id && !query.from.is_bot
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tgbot::ApprovalSignal;

    use super::*;

    const OWNER: UserId = UserId(5_000_000_001);

    fn message(user_id: u64, text: &str) -> Value {
        json!({
            "message_id": 42,
            "date": 1_700_000_000,
            "chat": {"id": -100123, "type": "supergroup", "title": "Shared chat"},
            "from": {"id": user_id, "is_bot": false, "first_name": "Sender"},
            "text": text
        })
    }

    fn incoming(payload: Value) -> IncomingMessage {
        IncomingMessage::from_message(serde_json::from_value(payload).unwrap(), "adapter_bot")
    }

    fn callback(user_id: u64, signal: ApprovalSignal) -> IncomingMessage {
        let mut approval_message = message(999, "Approve this request?");
        approval_message["from"]["is_bot"] = json!(true);
        IncomingMessage::from_callback_query(
            serde_json::from_value(json!({
                "id": "query-id",
                "from": {"id": user_id, "is_bot": false, "first_name": "Clicker"},
                "chat_instance": "instance",
                "message": approval_message,
                "data": signal.callback_data()
            }))
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn restricts_prompts_and_every_command_to_the_configured_sender() {
        for text in [
            "hello",
            "/init",
            "/new",
            "/delete",
            "/sessions",
            "/interrupt",
            "/exec task",
            "/unknown",
            "/init@adapter_bot",
        ] {
            assert!(is_allowed(Some(OWNER), &incoming(message(OWNER.0, text))));
            assert!(!is_allowed(Some(OWNER), &incoming(message(7, text))));
        }
    }

    #[test]
    fn compares_sender_id_in_private_chats_and_groups() {
        let mut payload = message(OWNER.0, "hello");
        assert!(is_allowed(Some(OWNER), &incoming(payload.clone())));
        payload["chat"] = json!({"id": OWNER.0, "type": "private", "first_name": "Owner"});
        assert!(is_allowed(Some(OWNER), &incoming(payload.clone())));
        payload["from"]["id"] = json!(7);
        assert!(!is_allowed(Some(OWNER), &incoming(payload)));
    }

    #[test]
    fn rejects_missing_senders_bots_and_messages_sent_as_a_chat() {
        let mut payload = message(OWNER.0, "/delete");
        payload.as_object_mut().unwrap().remove("from");
        assert!(!is_allowed(Some(OWNER), &incoming(payload)));

        let mut payload = message(OWNER.0, "/delete");
        payload["from"]["is_bot"] = json!(true);
        assert!(!is_allowed(Some(OWNER), &incoming(payload)));

        for chat in [
            json!({"id": -100123, "type": "supergroup", "title": "Anonymous admin"}),
            json!({"id": -100456, "type": "channel", "title": "Channel"}),
        ] {
            let mut payload = message(OWNER.0, "/delete");
            payload["sender_chat"] = chat;
            assert!(!is_allowed(Some(OWNER), &incoming(payload)));
        }
    }

    #[test]
    fn forwarded_messages_are_checked_against_the_person_sending_them() {
        let mut payload = message(7, "hello");
        payload["forward_origin"] = json!({
            "type": "user",
            "date": 1_700_000_000,
            "sender_user": {"id": OWNER.0, "is_bot": false, "first_name": "Owner"}
        });
        assert!(!is_allowed(Some(OWNER), &incoming(payload.clone())));
        payload["from"]["id"] = json!(OWNER.0);
        payload["forward_origin"]["sender_user"]["id"] = json!(7);
        assert!(is_allowed(Some(OWNER), &incoming(payload)));
    }

    #[test]
    fn approval_buttons_use_the_clickers_identity() {
        for signal in [ApprovalSignal::Allow, ApprovalSignal::Deny] {
            assert!(is_allowed(Some(OWNER), &callback(OWNER.0, signal)));
            assert!(!is_allowed(Some(OWNER), &callback(7, signal)));
            let mut incoming = callback(OWNER.0, signal);
            let IncomingMessage::ApprovalSignal { query, .. } = &mut incoming else {
                unreachable!();
            };
            query.from.is_bot = true;
            assert!(!is_allowed(Some(OWNER), &incoming));
        }
    }

    #[test]
    fn omitting_the_filter_preserves_unrestricted_behavior() {
        assert!(is_allowed(None, &incoming(message(7, "/delete"))));
        assert!(is_allowed(None, &callback(7, ApprovalSignal::Allow)));
        let mut payload = message(7, "hello");
        payload.as_object_mut().unwrap().remove("from");
        assert!(is_allowed(None, &incoming(payload)));
    }
}
