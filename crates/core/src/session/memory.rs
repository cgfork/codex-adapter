use std::{collections::HashMap, fmt::Debug, hash::Hash, sync::Arc};

use tokio::sync::RwLock;

use crate::BridgeResult;

use super::{SessionState, Store};

/// An in-process session store. Clones share records; reads return owned snapshots.
pub struct Memory<C> {
    records: Arc<RwLock<HashMap<C, SessionState>>>,
}

impl<C> Default for Memory<C> {
    fn default() -> Self {
        Self {
            records: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl<C> Clone for Memory<C> {
    fn clone(&self) -> Self {
        Self {
            records: Arc::clone(&self.records),
        }
    }
}

impl<C> Store for Memory<C>
where
    C: Clone + Debug + Eq + Hash + Send + Sync + 'static,
{
    type ChatId = C;
    type Session<'a> = SessionState;

    async fn load(&self, chat_id: &C) -> BridgeResult<Option<SessionState>> {
        Ok(self.records.read().await.get(chat_id).cloned())
    }

    async fn list(&self) -> BridgeResult<Vec<(C, SessionState)>> {
        Ok(self
            .records
            .read()
            .await
            .iter()
            .map(|(chat_id, state)| (chat_id.clone(), state.clone()))
            .collect())
    }

    async fn save(&self, chat_id: C, state: SessionState) -> BridgeResult<()> {
        self.records.write().await.insert(chat_id, state);
        Ok(())
    }

    async fn remove(&self, chat_id: &C) -> BridgeResult<()> {
        self.records.write().await.remove(chat_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use super::{Memory, SessionState, Store};

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct ChatId(u64);

    #[tokio::test]
    async fn absent_and_removed_records_are_none() {
        let store = Memory::<ChatId>::default();
        let chat = ChatId(1);
        assert_eq!(store.load(&chat).await.unwrap(), None);
        assert!(store.list().await.unwrap().is_empty());
        store.remove(&chat).await.unwrap();
        store
            .save(
                chat.clone(),
                SessionState {
                    session_ids: vec!["a".into()],
                },
            )
            .await
            .unwrap();
        store.remove(&chat).await.unwrap();
        assert_eq!(store.load(&chat).await.unwrap(), None);
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn clones_share_writes_but_reads_are_owned_snapshots() {
        let store = Memory::<ChatId>::default();
        let other = store.clone();
        let chat = ChatId(1);
        let second_chat = ChatId(2);
        store
            .save(
                chat.clone(),
                SessionState {
                    session_ids: vec!["first".into()],
                },
            )
            .await
            .unwrap();
        other
            .save(
                second_chat.clone(),
                SessionState {
                    session_ids: vec!["second".into()],
                },
            )
            .await
            .unwrap();

        let mut loaded = other.load(&chat).await.unwrap().unwrap();
        loaded.session_ids.push("local-only".into());
        let mut listed = store.list().await.unwrap();
        listed
            .iter_mut()
            .find(|(id, _)| *id == chat)
            .unwrap()
            .1
            .session_ids
            .clear();

        assert_eq!(
            store.load(&chat).await.unwrap().unwrap().session_ids,
            ["first"]
        );
        assert_eq!(
            other.load(&second_chat).await.unwrap().unwrap().session_ids,
            ["second"]
        );
        assert_eq!(store.list().await.unwrap().len(), 2);
    }

    #[test]
    fn default_does_not_require_chat_id_default() {
        fn accepts_memory<C: fmt::Debug>(_: &Memory<C>) {}
        accepts_memory(&Memory::<ChatId>::default());
    }
}
