#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use codexbridge::{AppServer, AppServerError, Codex};
use serde_json::json;

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    root: PathBuf,
    codex: Codex,
    _guard: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new(response_script: &str) -> Self {
        let guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "codexbridge app-server {} {timestamp} {serial}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let executable = root.join("fake codex");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\n\
                 IFS= read -r request || exit 1\n\
                 printf '%s\\n' '{{\"id\":1,\"result\":{{}}}}'\n\
                 IFS= read -r initialized || exit 1\n\
                 IFS= read -r request || exit 1\n\
                 {response_script}\n\
                 while IFS= read -r request; do :; done\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let codex = Codex::new(&root).with_executable(executable);
        Self {
            root,
            codex,
            _guard: guard,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn explicit_rpc_error_preserves_the_connection_for_another_turn() {
    let fixture = Fixture::new(
        "printf '%s\\n' '{\"id\":2,\"error\":{\"code\":-32602,\"message\":\"invalid prompt\"}}'\n\
         IFS= read -r request || exit 1\n\
         printf '%s\\n' '{\"id\":3,\"result\":{\"turn\":{\"id\":\"retry-turn\"}}}'",
    );
    let mut server = AppServer::start(&fixture.codex).await.unwrap();
    let error = server
        .start_turn("thread-1", "first prompt")
        .await
        .unwrap_err();
    let error = error.downcast_ref::<AppServerError>().unwrap();
    assert!(!error.is_connection_fatal());
    match error {
        AppServerError::Rpc { method, error } => {
            assert_eq!(method, "turn/start");
            assert_eq!(error["code"], -32602);
            assert_eq!(error["message"], "invalid prompt");
        }
        other => panic!("expected an RPC error, got {other:?}"),
    }
    assert_eq!(
        server.start_turn("thread-1", "retry prompt").await.unwrap(),
        "retry-turn"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn eof_and_malformed_json_are_connection_fatal() {
    for response_script in ["exit 0", "printf '%s\\n' '{invalid}'"] {
        let fixture = Fixture::new(response_script);
        let mut server = AppServer::start(&fixture.codex).await.unwrap();
        let error = server.start_turn("thread-1", "prompt").await.unwrap_err();
        let error = error.downcast_ref::<AppServerError>().unwrap();
        assert!(matches!(error, AppServerError::Transport { .. }));
        assert!(error.is_connection_fatal());
        assert!(std::error::Error::source(error).is_some());
        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn successful_rpc_response_without_a_result_is_connection_fatal() {
    let fixture = Fixture::new("printf '%s\\n' '{\"id\":2}'");
    let mut server = AppServer::start(&fixture.codex).await.unwrap();
    let error = server.request("turn/start", json!({})).await.unwrap_err();
    let error = error.downcast_ref::<AppServerError>().unwrap();
    assert!(matches!(
        error,
        AppServerError::InvalidResponse { method, message }
            if method == "turn/start" && *message == "response has no result"
    ));
    assert!(error.is_connection_fatal());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn successful_thread_response_requires_a_thread_id() {
    let fixture = Fixture::new("printf '%s\\n' '{\"id\":2,\"result\":{\"thread\":{}}}'");
    let mut server = AppServer::start(&fixture.codex).await.unwrap();
    let error = server.start_thread(&fixture.codex).await.unwrap_err();
    let error = error.downcast_ref::<AppServerError>().unwrap();
    assert!(matches!(
        error,
        AppServerError::InvalidResponse { method, message }
            if method == "thread/start" && *message == "response has no thread ID"
    ));
    assert!(error.is_connection_fatal());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn successful_turn_response_requires_a_turn_id() {
    let fixture = Fixture::new("printf '%s\\n' '{\"id\":2,\"result\":{\"turn\":{}}}'");
    let mut server = AppServer::start(&fixture.codex).await.unwrap();
    let error = server.start_turn("thread-1", "prompt").await.unwrap_err();
    let error = error.downcast_ref::<AppServerError>().unwrap();
    assert!(matches!(
        error,
        AppServerError::InvalidResponse { method, message }
            if method == "turn/start" && *message == "response has no turn ID"
    ));
    assert!(error.is_connection_fatal());
    server.shutdown().await.unwrap();
}
