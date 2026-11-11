use std::{
    collections::HashMap,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use serde_json::Value;
use teloxide::types::{ChatId, MessageId, UserId};

#[derive(Default)]
pub(crate) struct Sessions {
    pub(crate) chats: HashMap<ChatId, ChatSession>,
    pub(crate) approvals: HashMap<u64, PendingApproval>,
    next_approval: u64,
}

#[derive(Default)]
pub(crate) struct ChatSession {
    pub(crate) threads: Vec<String>,
    pub(crate) turn: Option<ActiveTurn>,
}

pub(crate) struct ActiveTurn {
    pub(crate) id: String,
    pub(crate) owner: Option<UserId>,
}

pub(crate) struct PendingApproval {
    pub(crate) chat_id: ChatId,
    pub(crate) message_id: Option<MessageId>,
    pub(crate) request_id: Value,
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
    pub(crate) owner: UserId,
    pub(crate) live: Arc<AtomicBool>,
}

impl Drop for PendingApproval {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
    }
}

impl ChatSession {
    pub(crate) fn active_thread(&self) -> Option<&str> {
        self.threads.last().map(String::as_str)
    }
}

impl Sessions {
    pub(crate) fn delete_current(&mut self, chat_id: ChatId) -> io::Result<Option<String>> {
        let Some(session) = self.chats.get_mut(&chat_id) else {
            return Ok(None);
        };
        if session.turn.is_some() {
            return Err(io::Error::other(
                "a turn is running; use /interrupt and wait for completion before /delete",
            ));
        }
        let thread_id = session.threads.pop();
        if session.threads.is_empty() {
            self.chats.remove(&chat_id);
        }
        if let Some(thread_id) = &thread_id {
            self.approvals
                .retain(|_, pending| pending.chat_id != chat_id || pending.thread_id != *thread_id);
        }
        Ok(thread_id)
    }

    pub(crate) fn chat_for_thread(&self, thread_id: &str) -> Option<ChatId> {
        self.chats.iter().find_map(|(chat_id, session)| {
            session
                .threads
                .iter()
                .any(|thread| thread == thread_id)
                .then_some(*chat_id)
        })
    }

    pub(crate) fn route(&self, event: &Value) -> Option<ChatId> {
        let params = &event["params"];
        let thread_id = params["threadId"].as_str()?;
        let chat_id = self.chat_for_thread(thread_id)?;
        let session = self.chats.get(&chat_id)?;
        if session.active_thread() != Some(thread_id) {
            return None;
        }
        let turn = session.turn.as_ref()?;
        let event_turn = params["turnId"]
            .as_str()
            .or_else(|| params["turn"]["id"].as_str());
        if event_turn.is_some_and(|turn_id| turn_id != turn.id) {
            return None;
        }
        Some(chat_id)
    }

    pub(crate) fn finish_turn(&mut self, event: &Value) -> Option<ChatId> {
        if event["method"] != "turn/completed" {
            return None;
        }
        let chat_id = self.route(event)?;
        let completed_id = event["params"]["turn"]["id"].as_str()?;
        if self.chats[&chat_id].turn.as_ref()?.id != completed_id {
            return None;
        }
        self.chats.get_mut(&chat_id)?.turn = None;
        self.approvals
            .retain(|_, pending| pending.chat_id != chat_id);
        Some(chat_id)
    }

    pub(crate) fn resolve_request(&mut self, event: &Value) {
        let params = &event["params"];
        self.approvals.retain(|_, pending| {
            !(params["threadId"].as_str() == Some(pending.thread_id.as_str())
                && params.get("requestId") == Some(&pending.request_id))
        });
    }

    pub(crate) fn queue_approval(
        &mut self,
        chat_id: ChatId,
        request_id: Value,
        thread_id: String,
        turn_id: String,
        owner: UserId,
    ) -> u64 {
        let ticket = self.next_approval;
        self.next_approval += 1;
        self.approvals.insert(
            ticket,
            PendingApproval {
                chat_id,
                message_id: None,
                request_id,
                thread_id,
                turn_id,
                owner,
                live: Arc::new(AtomicBool::new(true)),
            },
        );
        ticket
    }

    pub(crate) fn register_delivery(&mut self, ticket: u64, message_id: MessageId) -> bool {
        let Some(pending) = self.approvals.get(&ticket) else {
            return false;
        };
        if !self.pending_is_current(pending) || pending.message_id.is_some() {
            return false;
        }
        self.approvals.get_mut(&ticket).unwrap().message_id = Some(message_id);
        true
    }

    pub(crate) fn pending_is_current(&self, pending: &PendingApproval) -> bool {
        let Some(session) = self.chats.get(&pending.chat_id) else {
            return false;
        };
        session.active_thread() == Some(pending.thread_id.as_str())
            && session
                .turn
                .as_ref()
                .is_some_and(|turn| turn.id == pending.turn_id)
    }

    pub(crate) fn approval_ticket(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        user_id: UserId,
    ) -> Option<u64> {
        self.approvals.iter().find_map(|(ticket, pending)| {
            (pending.chat_id == chat_id
                && pending.message_id == Some(message_id)
                && pending.owner == user_id
                && self.pending_is_current(pending))
            .then_some(*ticket)
        })
    }

    #[cfg(test)]
    fn approval_is_current(&self, chat_id: ChatId, message_id: MessageId, user_id: UserId) -> bool {
        self.approval_ticket(chat_id, message_id, user_id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ActiveTurn, ChatSession, Sessions};
    use teloxide::types::{ChatId, MessageId, UserId};

    fn sessions() -> Sessions {
        let mut sessions = Sessions::default();
        for (chat, thread, turn) in [(1, "thread-1", "turn-1"), (2, "thread-2", "turn-2")] {
            sessions.chats.insert(
                ChatId(chat),
                ChatSession {
                    threads: vec![format!("old-{thread}"), thread.into()],
                    turn: Some(ActiveTurn {
                        id: turn.into(),
                        owner: Some(UserId(7)),
                    }),
                },
            );
        }
        let ticket = sessions.queue_approval(
            ChatId(1),
            json!(42),
            "thread-1".into(),
            "turn-1".into(),
            UserId(7),
        );
        assert!(sessions.register_delivery(ticket, MessageId(10)));
        sessions
    }

    #[test]
    fn deleting_current_session_preserves_earlier_sessions_and_other_chats() {
        let mut sessions = sessions();
        sessions.chats.get_mut(&ChatId(1)).unwrap().turn = None;
        let other_ticket = sessions.queue_approval(
            ChatId(2),
            json!(43),
            "thread-2".into(),
            "turn-2".into(),
            UserId(7),
        );
        assert!(sessions.register_delivery(other_ticket, MessageId(11)));

        assert_eq!(
            sessions.delete_current(ChatId(1)).unwrap().as_deref(),
            Some("thread-1")
        );
        assert_eq!(sessions.chats[&ChatId(1)].threads, ["old-thread-1"]);
        assert_eq!(
            sessions.chats[&ChatId(1)].active_thread(),
            Some("old-thread-1")
        );
        assert!(sessions.chats[&ChatId(1)].turn.is_none());
        assert_eq!(sessions.chat_for_thread("thread-1"), None);
        assert_eq!(
            sessions.chats[&ChatId(2)].threads,
            ["old-thread-2", "thread-2"]
        );
        assert_eq!(
            sessions.route(&json!({"params":{"threadId":"thread-2","turnId":"turn-2"}})),
            Some(ChatId(2))
        );
        assert_eq!(sessions.approvals.len(), 1);
        assert!(sessions.approval_is_current(ChatId(2), MessageId(11), UserId(7)));
    }

    #[test]
    fn deleting_last_session_removes_the_chat_entry() {
        let mut sessions = sessions();
        sessions.chats.get_mut(&ChatId(1)).unwrap().turn = None;
        sessions.delete_current(ChatId(1)).unwrap();
        assert_eq!(
            sessions.delete_current(ChatId(1)).unwrap().as_deref(),
            Some("old-thread-1")
        );
        assert!(!sessions.chats.contains_key(&ChatId(1)));
        assert_eq!(sessions.chat_for_thread("old-thread-1"), None);
        assert_eq!(sessions.delete_current(ChatId(1)).unwrap(), None);
        assert!(sessions.chats.contains_key(&ChatId(2)));
    }

    #[test]
    fn deleting_a_missing_or_empty_session_does_not_create_a_session() {
        let mut sessions = Sessions::default();
        assert_eq!(sessions.delete_current(ChatId(1)).unwrap(), None);
        assert!(sessions.chats.is_empty());
        sessions.chats.entry(ChatId(1)).or_default();
        assert_eq!(sessions.delete_current(ChatId(1)).unwrap(), None);
        assert!(sessions.chats.is_empty());
    }

    #[test]
    fn deleting_a_running_session_is_rejected_without_changing_state() {
        let mut sessions = sessions();
        let error = sessions.delete_current(ChatId(1)).unwrap_err();
        assert!(error.to_string().contains("/interrupt"));
        assert!(error.to_string().contains("/delete"));
        assert_eq!(
            sessions.chats[&ChatId(1)].threads,
            ["old-thread-1", "thread-1"]
        );
        assert_eq!(
            sessions.route(&json!({"params":{"threadId":"thread-1","turnId":"turn-1"}})),
            Some(ChatId(1))
        );
        assert!(sessions.approval_is_current(ChatId(1), MessageId(10), UserId(7)));
        assert!(
            sessions
                .approvals
                .values()
                .all(|pending| { pending.live.load(std::sync::atomic::Ordering::Acquire) })
        );

        sessions.finish_turn(&json!({"method":"turn/completed","params":{
            "threadId":"thread-1","turn":{"id":"turn-1","status":"interrupted"}
        }}));
        assert_eq!(
            sessions.delete_current(ChatId(1)).unwrap().as_deref(),
            Some("thread-1")
        );
    }

    #[test]
    fn deleted_sessions_cannot_route_events_or_reactivate_late_approvals() {
        let mut sessions = sessions();
        let ticket = sessions.queue_approval(
            ChatId(1),
            json!(43),
            "thread-1".into(),
            "turn-1".into(),
            UserId(7),
        );
        let live = sessions.approvals[&ticket].live.clone();
        sessions.chats.get_mut(&ChatId(1)).unwrap().turn = None;
        sessions.delete_current(ChatId(1)).unwrap();
        sessions.chats.get_mut(&ChatId(1)).unwrap().turn = Some(ActiveTurn {
            id: "new-turn".into(),
            owner: Some(UserId(7)),
        });

        assert!(!live.load(std::sync::atomic::Ordering::Acquire));
        assert!(!sessions.register_delivery(ticket, MessageId(11)));
        assert!(!sessions.approval_is_current(ChatId(1), MessageId(10), UserId(7)));
        assert!(!sessions.approval_is_current(ChatId(1), MessageId(11), UserId(7)));
        assert_eq!(
            sessions.route(&json!({"params":{"threadId":"thread-1","turnId":"turn-1"}})),
            None
        );
        assert_eq!(
            sessions.finish_turn(&json!({"method":"turn/completed","params":{
                "threadId":"thread-1","turn":{"id":"turn-1"}
            }})),
            None
        );
        assert_eq!(
            sessions.route(&json!({"params":{"threadId":"old-thread-1","turnId":"new-turn"}})),
            Some(ChatId(1))
        );
    }

    #[test]
    fn routes_only_the_chat_and_turn_that_originated_the_event() {
        let sessions = sessions();
        let event = json!({"params":{"threadId":"thread-2","turnId":"turn-2"}});
        assert_eq!(sessions.route(&event), Some(ChatId(2)));
        for (thread, turn) in [
            ("thread-1", "old-turn"),
            ("old-thread-1", "turn-1"),
            ("unknown", "turn-1"),
        ] {
            assert_eq!(
                sessions.route(&json!({"params":{"threadId":thread,"turnId":turn}})),
                None
            );
        }
        assert_eq!(sessions.chats[&ChatId(1)].threads.len(), 2);
    }

    #[test]
    fn completion_clears_only_the_matching_turn_and_its_approvals() {
        let mut sessions = sessions();
        let stale = json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"old-turn"}}});
        assert_eq!(sessions.finish_turn(&stale), None);
        assert!(sessions.chats[&ChatId(1)].turn.is_some());
        let complete = json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1"}}});
        assert_eq!(sessions.finish_turn(&complete), Some(ChatId(1)));
        assert!(sessions.chats[&ChatId(1)].turn.is_none());
        assert!(sessions.chats[&ChatId(2)].turn.is_some());
        assert!(sessions.approvals.is_empty());
        assert_eq!(
            sessions.route(&json!({"params":{"threadId":"thread-1"}})),
            None
        );
    }

    #[test]
    fn approvals_bind_chat_message_user_and_turn() {
        let mut sessions = sessions();
        assert!(sessions.approval_is_current(ChatId(1), MessageId(10), UserId(7)));
        assert!(!sessions.approval_is_current(ChatId(2), MessageId(10), UserId(7)));
        assert!(!sessions.approval_is_current(ChatId(1), MessageId(11), UserId(7)));
        assert!(!sessions.approval_is_current(ChatId(1), MessageId(10), UserId(8)));
        sessions
            .chats
            .get_mut(&ChatId(1))
            .unwrap()
            .turn
            .as_mut()
            .unwrap()
            .id = "new-turn".into();
        assert!(!sessions.approval_is_current(ChatId(1), MessageId(10), UserId(7)));
    }

    #[test]
    fn server_resolution_expires_the_exact_request() {
        let mut sessions = sessions();
        sessions.resolve_request(&json!({"params":{"threadId":"thread-2","requestId":42}}));
        assert_eq!(sessions.approvals.len(), 1);
        sessions.resolve_request(&json!({"params":{"threadId":"thread-1","requestId":42}}));
        assert!(sessions.approvals.is_empty());
        let ticket = sessions.queue_approval(
            ChatId(1),
            json!(43),
            "thread-1".into(),
            "turn-1".into(),
            UserId(7),
        );
        assert!(sessions.register_delivery(ticket, MessageId(11)));
        assert!(!sessions.approval_is_current(ChatId(1), MessageId(10), UserId(7)));
        assert!(sessions.approval_is_current(ChatId(1), MessageId(11), UserId(7)));
    }

    #[test]
    fn approval_delivered_after_turn_completion_cannot_become_active() {
        let mut sessions = sessions();
        let ticket = sessions.queue_approval(
            ChatId(1),
            json!(43),
            "thread-1".into(),
            "turn-1".into(),
            UserId(7),
        );
        let live = sessions.approvals[&ticket].live.clone();
        sessions.finish_turn(&json!({"method":"turn/completed","params":{
            "threadId":"thread-1","turn":{"id":"turn-1"}
        }}));
        assert!(!live.load(std::sync::atomic::Ordering::Acquire));
        assert!(!sessions.register_delivery(ticket, MessageId(11)));
        assert!(!sessions.approval_is_current(ChatId(1), MessageId(11), UserId(7)));
    }

    #[test]
    fn resolved_delivery_cannot_attach_to_a_new_request_with_the_same_rpc_id() {
        let mut sessions = sessions();
        let old_ticket = sessions.queue_approval(
            ChatId(1),
            json!(43),
            "thread-1".into(),
            "turn-1".into(),
            UserId(7),
        );
        sessions.resolve_request(&json!({"params":{"threadId":"thread-1","requestId":43}}));
        let new_ticket = sessions.queue_approval(
            ChatId(1),
            json!(43),
            "thread-1".into(),
            "turn-1".into(),
            UserId(7),
        );
        assert_ne!(old_ticket, new_ticket);
        assert!(!sessions.register_delivery(old_ticket, MessageId(11)));
        assert!(sessions.register_delivery(new_ticket, MessageId(12)));
        assert!(!sessions.approval_is_current(ChatId(1), MessageId(11), UserId(7)));
        assert!(sessions.approval_is_current(ChatId(1), MessageId(12), UserId(7)));
    }
}
