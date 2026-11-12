use std::{collections::HashMap, io};

use codebridge_core::{BridgeResult, Session, SessionState, Store};

struct BorrowedStore {
    records: HashMap<u64, SessionState>,
}

impl Store for BorrowedStore {
    type ChatId = u64;
    type Session<'a> = &'a SessionState;

    fn load<'a>(
        &'a self,
        chat_id: &u64,
    ) -> impl Future<Output = BridgeResult<Option<Self::Session<'a>>>> + Send {
        let session = self.records.get(chat_id);
        async move { Ok(session) }
    }

    fn list(&self) -> impl Future<Output = BridgeResult<Vec<(u64, Self::Session<'_>)>>> + Send {
        let sessions = self
            .records
            .iter()
            .map(|(id, state)| (*id, state))
            .collect();
        async move { Ok(sessions) }
    }

    async fn save(&self, _: u64, _: SessionState) -> BridgeResult<()> {
        Err(io::Error::other("read-only store").into())
    }

    async fn remove(&self, _: &u64) -> BridgeResult<()> {
        Err(io::Error::other("read-only store").into())
    }
}

#[tokio::test]
async fn external_store_can_return_borrowed_session_handles() {
    let store = BorrowedStore {
        records: HashMap::from([
            (
                7,
                SessionState {
                    session_ids: vec!["first".into(), "active".into()],
                },
            ),
            (
                8,
                SessionState {
                    session_ids: vec!["other".into()],
                },
            ),
        ]),
    };

    let handle = store.load(&7).await.unwrap().unwrap();
    assert_eq!(handle.session_ids(), ["first", "active"]);
    assert_eq!(Session::active_session(&handle), Some("active"));
    let mut owned = handle.to_state();
    owned.session_ids.push("local".into());
    assert_eq!(
        store.load(&7).await.unwrap().unwrap().session_ids(),
        ["first", "active"]
    );
    assert!(store.load(&9).await.unwrap().is_none());

    let listed = store.list().await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(
        listed
            .iter()
            .find(|(id, _)| *id == 8)
            .unwrap()
            .1
            .active_session(),
        Some("other")
    );
    assert!(store.save(7, owned).await.is_err());
    assert_eq!(
        store.load(&7).await.unwrap().unwrap().session_ids(),
        ["first", "active"]
    );
}
