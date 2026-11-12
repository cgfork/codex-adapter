use std::{
    collections::HashMap,
    hash::Hash,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(crate) struct ChatSession<U> {
    pub threads: Vec<String>,
    pub turn: Option<ActiveTurn<U>>,
}

impl<U> Default for ChatSession<U> {
    fn default() -> Self {
        Self {
            threads: Vec::new(),
            turn: None,
        }
    }
}

impl<U> ChatSession<U> {
    pub fn active_thread(&self) -> Option<&str> {
        self.threads.last().map(String::as_str)
    }
}

pub(crate) struct ActiveTurn<U> {
    pub id: String,
    pub owner: Option<U>,
}

pub(crate) struct PendingApproval<C, U, M> {
    pub chat_id: C,
    pub message_id: Option<M>,
    pub request_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub owner: U,
    pub live: Arc<AtomicBool>,
}

impl<C, U, M> Drop for PendingApproval<C, U, M> {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
    }
}

pub(crate) struct Sessions<C, U, M> {
    pub chats: HashMap<C, ChatSession<U>>,
    pub approvals: HashMap<u64, PendingApproval<C, U, M>>,
    next_approval: u64,
}

impl<C, U, M> Default for Sessions<C, U, M> {
    fn default() -> Self {
        Self {
            chats: HashMap::new(),
            approvals: HashMap::new(),
            next_approval: 0,
        }
    }
}

impl<C: Clone + Eq + Hash, U: Eq, M: Eq> Sessions<C, U, M> {
    pub fn delete_current(&mut self, chat_id: &C) -> io::Result<Option<String>> {
        let Some(session) = self.chats.get_mut(chat_id) else {
            return Ok(None);
        };
        if session.turn.is_some() {
            return Err(io::Error::other(
                "a turn is running; use /interrupt and wait for completion before /delete",
            ));
        }
        let thread_id = session.threads.pop();
        if session.threads.is_empty() {
            self.chats.remove(chat_id);
        }
        if let Some(thread_id) = &thread_id {
            self.approvals.retain(|_, pending| {
                &pending.chat_id != chat_id || &pending.thread_id != thread_id
            });
        }
        Ok(thread_id)
    }

    pub fn chat_for_thread(&self, thread_id: &str) -> Option<C> {
        self.chats.iter().find_map(|(chat, session)| {
            session
                .threads
                .iter()
                .any(|thread| thread == thread_id)
                .then(|| chat.clone())
        })
    }

    pub fn route(&self, thread_id: &str, turn_id: Option<&str>) -> Option<C> {
        let chat = self.chat_for_thread(thread_id)?;
        let session = self.chats.get(&chat)?;
        if session.active_thread() != Some(thread_id) {
            return None;
        }
        let turn = session.turn.as_ref()?;
        if turn_id.is_some_and(|id| id != turn.id) {
            return None;
        }
        Some(chat)
    }

    pub fn finish_turn(&mut self, thread_id: &str, turn_id: &str) -> Option<C> {
        let chat = self.route(thread_id, Some(turn_id))?;
        self.chats.get_mut(&chat)?.turn = None;
        self.approvals.retain(|_, pending| pending.chat_id != chat);
        Some(chat)
    }

    pub fn resolve_request(&mut self, thread_id: &str, request_id: &str) -> Vec<(C, M)> {
        let mut cleared = Vec::new();
        self.approvals.retain(|_, pending| {
            if pending.thread_id == thread_id && pending.request_id == request_id {
                if let Some(message) = pending.message_id.take() {
                    cleared.push((pending.chat_id.clone(), message));
                }
                false
            } else {
                true
            }
        });
        cleared
    }

    pub fn queue_approval(
        &mut self,
        chat_id: C,
        request_id: String,
        thread_id: String,
        turn_id: String,
        owner: U,
    ) -> (u64, Arc<AtomicBool>) {
        let ticket = self.next_approval;
        self.next_approval = self
            .next_approval
            .checked_add(1)
            .expect("approval ticket exhausted");
        let live = Arc::new(AtomicBool::new(true));
        self.approvals.insert(
            ticket,
            PendingApproval {
                chat_id,
                message_id: None,
                request_id,
                thread_id,
                turn_id,
                owner,
                live: live.clone(),
            },
        );
        (ticket, live)
    }

    pub fn register_delivery(&mut self, ticket: u64, message_id: M) -> bool {
        let Some(pending) = self.approvals.get(&ticket) else {
            return false;
        };
        if !self.pending_is_current(pending) || pending.message_id.is_some() {
            return false;
        }
        self.approvals
            .get_mut(&ticket)
            .expect("pending still present")
            .message_id = Some(message_id);
        true
    }

    pub fn pending_is_current(&self, pending: &PendingApproval<C, U, M>) -> bool {
        self.chats.get(&pending.chat_id).is_some_and(|session| {
            session.active_thread() == Some(pending.thread_id.as_str())
                && session
                    .turn
                    .as_ref()
                    .is_some_and(|turn| turn.id == pending.turn_id)
        })
    }

    pub fn approval_ticket(&self, chat: &C, message: &M, user: &U) -> Option<u64> {
        self.approvals.iter().find_map(|(ticket, pending)| {
            (&pending.chat_id == chat
                && pending.message_id.as_ref() == Some(message)
                && &pending.owner == user
                && self.pending_is_current(pending))
            .then_some(*ticket)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::{ActiveTurn, ChatSession, Sessions};

    #[test]
    fn deleting_current_session_preserves_previous_session_and_other_chat() {
        let mut sessions = Sessions::<u64, u64, u64>::default();
        sessions.chats.insert(
            1,
            ChatSession {
                threads: vec!["old".into(), "current".into()],
                turn: None,
            },
        );
        sessions.chats.insert(
            2,
            ChatSession {
                threads: vec!["other".into()],
                turn: Some(ActiveTurn {
                    id: "turn".into(),
                    owner: Some(7),
                }),
            },
        );
        assert_eq!(
            sessions.delete_current(&1).unwrap().as_deref(),
            Some("current")
        );
        assert_eq!(sessions.chats[&1].active_thread(), Some("old"));
        assert_eq!(sessions.chats[&2].active_thread(), Some("other"));
        assert_eq!(sessions.chat_for_thread("current"), None);
        assert_eq!(sessions.delete_current(&1).unwrap().as_deref(), Some("old"));
        assert!(!sessions.chats.contains_key(&1));
        assert!(sessions.delete_current(&2).is_err());
    }

    #[test]
    fn expired_delivery_cannot_reactivate_same_request_id() {
        let mut sessions = Sessions::<u64, u64, u64>::default();
        sessions.chats.insert(
            1,
            ChatSession {
                threads: vec!["session".into()],
                turn: Some(ActiveTurn {
                    id: "turn".into(),
                    owner: Some(7),
                }),
            },
        );
        let (old, live) =
            sessions.queue_approval(1, "request".into(), "session".into(), "turn".into(), 7);
        assert!(sessions.resolve_request("session", "request").is_empty());
        assert!(!live.load(Ordering::Acquire));
        let (new, _) =
            sessions.queue_approval(1, "request".into(), "session".into(), "turn".into(), 7);
        assert_ne!(old, new);
        assert!(!sessions.register_delivery(old, 10));
        assert!(sessions.register_delivery(new, 11));
        assert_eq!(sessions.approval_ticket(&1, &10, &7), None);
        assert_eq!(sessions.approval_ticket(&1, &11, &7), Some(new));
    }

    #[test]
    fn completing_turn_invalidates_approvals_and_rejects_stale_events() {
        let mut sessions = Sessions::<u64, u64, u64>::default();
        sessions.chats.insert(
            1,
            ChatSession {
                threads: vec!["old".into(), "active".into()],
                turn: Some(ActiveTurn {
                    id: "turn".into(),
                    owner: Some(7),
                }),
            },
        );
        let (ticket, live) =
            sessions.queue_approval(1, "request".into(), "active".into(), "turn".into(), 7);
        assert!(sessions.register_delivery(ticket, 10));
        assert_eq!(sessions.route("old", Some("turn")), None);
        assert_eq!(sessions.route("active", Some("stale")), None);
        assert_eq!(sessions.finish_turn("active", "stale"), None);
        assert!(live.load(Ordering::Acquire));
        assert_eq!(sessions.finish_turn("active", "turn"), Some(1));
        assert!(!live.load(Ordering::Acquire));
        assert!(sessions.approvals.is_empty());
        assert_eq!(sessions.route("active", None), None);
        assert_eq!(sessions.approval_ticket(&1, &10, &7), None);
    }
}
