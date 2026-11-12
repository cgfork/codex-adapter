//! Storage of a chat's session history. Active turns and approvals are transient.

use std::{fmt::Debug, future::Future, hash::Hash};

use crate::BridgeResult;

mod memory;
pub use memory::Memory;

/// Read access to a store's session history, whether borrowed or owned.
pub trait Session {
    fn session_ids(&self) -> &[String];

    fn active_session(&self) -> Option<&str> {
        self.session_ids().last().map(String::as_str)
    }

    /// Copy the history into a value suitable for an atomic store update.
    fn to_state(&self) -> SessionState {
        SessionState {
            session_ids: self.session_ids().to_vec(),
        }
    }
}

impl<S: Session + ?Sized> Session for &S {
    fn session_ids(&self) -> &[String] {
        (**self).session_ids()
    }
}

/// Ordered agent session IDs for a chat. The last ID is the active session.
/// Used as an owned snapshot and as the input to [`Store::save`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionState {
    pub session_ids: Vec<String>,
}

impl SessionState {
    pub fn active_session(&self) -> Option<&str> {
        self.session_ids.last().map(String::as_str)
    }
}

impl Session for SessionState {
    fn session_ids(&self) -> &[String] {
        &self.session_ids
    }
}

/// A pluggable store of session histories with async I/O.
///
/// [`Store::Session`] is a generic associated type tied to the store borrow.
/// Implementations may return borrowed views, lock guards, or owned snapshots.
///
/// A bridge loads a snapshot with `list` at startup and writes each history change
/// before acknowledging it. Use one active bridge writer per store. External
/// writes are picked up on restart; live turns, approvals, and independent tasks
/// are never persisted. Failed writes must leave the previous record intact.
/// Futures may be cancelled on shutdown; a write must still be atomic if its
/// caller stops awaiting it. Each session ID must belong to exactly one chat.
pub trait Store: Send + Sync + 'static {
    type ChatId: Clone + Debug + Eq + Hash + Send + Sync + 'static;

    type Session<'a>: Session + Send + 'a
    where
        Self: 'a;

    /// Borrow or snapshot a chat's history; return `None` when it is absent.
    fn load<'a>(
        &'a self,
        chat_id: &Self::ChatId,
    ) -> impl Future<Output = BridgeResult<Option<Self::Session<'a>>>> + Send;

    /// Return one snapshot containing at most one record per chat.
    fn list(
        &self,
    ) -> impl Future<Output = BridgeResult<Vec<(Self::ChatId, Self::Session<'_>)>>> + Send;

    /// Atomically insert or replace a chat's history.
    fn save(
        &self,
        chat_id: Self::ChatId,
        state: SessionState,
    ) -> impl Future<Output = BridgeResult<()>> + Send;

    /// Remove a chat's history. Removing an absent record succeeds.
    fn remove(&self, chat_id: &Self::ChatId) -> impl Future<Output = BridgeResult<()>> + Send;
}
