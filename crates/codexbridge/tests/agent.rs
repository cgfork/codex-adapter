#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use codebridge_core::{AgentEvent, AgentOutput, ApprovalDecision, CodeAgent};
use codexbridge::{AppServer, AppServerError, Codex, CodexAgent};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    root: PathBuf,
    codex: Codex,
    _guard: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new(script: &str) -> Self {
        let guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "codexbridge-agent-{}-{timestamp}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let executable = root.join("codex");
        fs::write(&executable, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            codex: Codex::new(&root).with_executable(executable),
            root,
            _guard: guard,
        }
    }

    async fn agent(&self) -> CodexAgent {
        let server = AppServer::start(&self.codex).await.unwrap();
        CodexAgent::new(self.codex.clone(), server)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const INITIALIZE: &str = "IFS= read -r request || exit 1\nprintf '%s\\n' '{\"id\":1,\"result\":{}}'\nIFS= read -r initialized || exit 1";

#[tokio::test]
async fn rpc_ids_keep_numeric_and_string_identity_in_approval_and_rejection() {
    let fixture = Fixture::new(&format!(
        "{INITIALIZE}\n\
         printf '%s\\n' '{{\"id\":42,\"method\":\"item/commandExecution/requestApproval\",\"params\":{{\"threadId\":\"s\",\"turnId\":\"t\",\"command\":\"cargo test\"}}}}'\n\
         printf '%s\\n' '{{\"id\":\"42\",\"method\":\"item/fileChange/requestApproval\",\"params\":{{\"threadId\":\"s\",\"turnId\":\"t\"}}}}'\n\
         printf '%s\\n' '{{\"id\":43,\"method\":\"unknown/request\",\"params\":{{\"threadId\":\"s\"}}}}'\n\
         IFS= read -r first || exit 1\n\
         IFS= read -r second || exit 1\n\
         IFS= read -r third || exit 1\n\
         printf '%s\\n' \"$first\" \"$second\" \"$third\" > responses"
    ));
    let mut agent = fixture.agent().await;
    let mut ids = Vec::new();
    for _ in 0..2 {
        let event = timeout(Duration::from_secs(5), agent.next_event())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        match event {
            AgentEvent::ApprovalRequested {
                request_id,
                description,
                ..
            } => {
                assert!(description.contains("Only the user who started this turn"));
                ids.push(request_id);
            }
            other => panic!("expected approval, got {other:?}"),
        }
    }
    assert_eq!(ids, ["42", "\"42\""]);
    agent
        .respond_approval(&ids[0], ApprovalDecision::Allow)
        .await
        .unwrap();
    agent
        .respond_approval(&ids[1], ApprovalDecision::Deny)
        .await
        .unwrap();
    let event = agent.next_event().await.unwrap().unwrap();
    let AgentEvent::UnsupportedRequest { request_id, .. } = event else {
        panic!("expected unsupported request");
    };
    agent.reject_request(&request_id).await.unwrap();
    agent.shutdown().await.unwrap();
    let responses: Vec<Value> = fs::read_to_string(fixture.root.join("responses"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        responses[0],
        json!({"id":42,"result":{"decision":"accept"}})
    );
    assert_eq!(
        responses[1],
        json!({"id":"42","result":{"decision":"decline"}})
    );
    assert_eq!(responses[2]["id"], 43);
    assert_eq!(responses[2]["error"]["code"], -32601);
}

#[tokio::test]
async fn independent_exec_reports_structured_output_and_exit_failure() {
    let fixture = Fixture::new(&format!(
        "if [ \"$1\" = exec ]; then\n\
           cat >/dev/null\n\
           printf '%s\\n' '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"answer\"}}}}'\n\
           printf '%s\\n' '{{\"type\":\"turn.failed\",\"error\":{{\"message\":\"failed\"}}}}'\n\
           printf '%s' 'diagnostic' >&2\n\
           exit 7\n\
         fi\n{INITIALIZE}\nwhile IFS= read -r request; do :; done"
    ));
    let mut agent = fixture.agent().await;
    let output = agent.exec("task".into()).await.unwrap();
    assert!(!output.success);
    assert!(output.status.contains('7'));
    assert_eq!(output.stderr, "diagnostic");
    assert_eq!(
        output.output,
        vec![
            AgentOutput::Message("answer".into()),
            AgentOutput::Error("failed".into()),
        ]
    );
    agent.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_independent_exec_stops_and_reaps_child() {
    let fixture = Fixture::new(&format!(
        "if [ \"$1\" = exec ]; then\n\
           printf '%s' \"$$\" > pid\n\
           exec sleep 30\n\
         fi\n{INITIALIZE}\nwhile IFS= read -r request; do :; done"
    ));
    let mut agent = fixture.agent().await;
    let task = tokio::spawn(agent.exec("task".into()));
    timeout(Duration::from_secs(5), async {
        while !fixture.root.join("pid").exists() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    let pid = fs::read_to_string(fixture.root.join("pid")).unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if !Command::new("kill")
                .args(["-0", &pid])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelled exec child was not reaped");
    agent.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_exec_json_reports_line_number_and_rpc_rejection_is_recoverable() {
    let fixture = Fixture::new(&format!(
        "if [ \"$1\" = exec ]; then\n\
           cat >/dev/null\n\
           printf '%s\\n' '{{}}' '{{invalid}}'\n\
           exit 0\n\
         fi\n{INITIALIZE}\n\
         IFS= read -r request || exit 1\n\
         printf '%s\\n' '{{\"id\":2,\"error\":{{\"code\":-32602,\"message\":\"bad prompt\"}}}}'\n\
         while IFS= read -r request; do :; done"
    ));
    let mut agent = fixture.agent().await;
    let output = agent.exec("task".into()).await.unwrap();
    assert!(output.success);
    assert!(
        matches!(&output.output[..], [AgentOutput::Error(message)] if message.contains("line 2"))
    );
    let error = agent.start_turn("s", "prompt").await.unwrap_err();
    assert!(matches!(
        error.downcast_ref::<AppServerError>(),
        Some(AppServerError::Rpc { .. })
    ));
    assert!(!agent.is_connection_fatal(&error));
    agent.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_exec_output_keeps_failed_status_and_stderr() {
    let fixture = Fixture::new(&format!(
        "if [ \"$1\" = exec ]; then\n\
           cat >/dev/null\n\
           printf '%s\\n' '{{invalid}}'\n\
           printf '%s' 'process diagnostic' >&2\n\
           exit 7\n\
         fi\n{INITIALIZE}\nwhile IFS= read -r request; do :; done"
    ));
    let mut agent = fixture.agent().await;
    let output = agent.exec("task".into()).await.unwrap();
    assert!(!output.success);
    assert!(output.status.contains('7'));
    assert_eq!(output.stderr, "process diagnostic");
    assert!(
        matches!(&output.output[..], [AgentOutput::Error(message)] if message.contains("line 1"))
    );
    agent.shutdown().await.unwrap();
}

#[tokio::test]
async fn stored_session_resumes_once_before_starting_turns() {
    let fixture = Fixture::new(&format!(
        "{INITIALIZE}\n\
         IFS= read -r request || exit 1\n\
         printf '%s\\n' \"$request\" >> requests\n\
         printf '%s\\n' '{{\"id\":2,\"result\":{{\"thread\":{{\"id\":\"stored\"}}}}}}'\n\
         IFS= read -r request || exit 1\n\
         printf '%s\\n' \"$request\" >> requests\n\
         printf '%s\\n' '{{\"id\":3,\"result\":{{\"turn\":{{\"id\":\"first\"}}}}}}'\n\
         IFS= read -r request || exit 1\n\
         printf '%s\\n' \"$request\" >> requests\n\
         printf '%s\\n' '{{\"id\":4,\"result\":{{\"turn\":{{\"id\":\"second\"}}}}}}'\n\
         while IFS= read -r request; do :; done"
    ));
    let mut agent = fixture.agent().await;
    assert_eq!(agent.start_turn("stored", "one").await.unwrap(), "first");
    assert_eq!(agent.start_turn("stored", "two").await.unwrap(), "second");
    agent.shutdown().await.unwrap();
    let requests: Vec<Value> = fs::read_to_string(fixture.root.join("requests"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        requests
            .iter()
            .map(|request| request["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["thread/resume", "turn/start", "turn/start"]
    );
    assert_eq!(
        requests[0]["params"],
        json!({"threadId":"stored","excludeTurns":true})
    );
}

#[tokio::test]
async fn newly_started_session_skips_resume() {
    let fixture = Fixture::new(&format!(
        "{INITIALIZE}\n\
         IFS= read -r request || exit 1\n\
         printf '%s\\n' \"$request\" >> requests\n\
         printf '%s\\n' '{{\"id\":2,\"result\":{{\"thread\":{{\"id\":\"new\"}}}}}}'\n\
         IFS= read -r request || exit 1\n\
         printf '%s\\n' \"$request\" >> requests\n\
         printf '%s\\n' '{{\"id\":3,\"result\":{{\"turn\":{{\"id\":\"turn\"}}}}}}'\n\
         while IFS= read -r request; do :; done"
    ));
    let mut agent = fixture.agent().await;
    assert_eq!(agent.start_session().await.unwrap(), "new");
    assert_eq!(agent.start_turn("new", "prompt").await.unwrap(), "turn");
    agent.shutdown().await.unwrap();
    let requests: Vec<Value> = fs::read_to_string(fixture.root.join("requests"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        requests
            .iter()
            .map(|request| request["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["thread/start", "turn/start"]
    );
}

#[tokio::test]
async fn failed_resume_is_recoverable_and_retried_before_turn() {
    let fixture = Fixture::new(&format!(
        "{INITIALIZE}\n\
         IFS= read -r request || exit 1\n\
         printf '%s\\n' \"$request\" >> requests\n\
         printf '%s\\n' '{{\"id\":2,\"error\":{{\"code\":-32000,\"message\":\"not loaded\"}}}}'\n\
         IFS= read -r request || exit 1\n\
         printf '%s\\n' \"$request\" >> requests\n\
         printf '%s\\n' '{{\"id\":3,\"result\":{{\"thread\":{{\"id\":\"stored\"}}}}}}'\n\
         IFS= read -r request || exit 1\n\
         printf '%s\\n' \"$request\" >> requests\n\
         printf '%s\\n' '{{\"id\":4,\"result\":{{\"turn\":{{\"id\":\"turn\"}}}}}}'\n\
         while IFS= read -r request; do :; done"
    ));
    let mut agent = fixture.agent().await;
    let error = agent.start_turn("stored", "first").await.unwrap_err();
    assert!(!agent.is_connection_fatal(&error));
    assert_eq!(agent.start_turn("stored", "retry").await.unwrap(), "turn");
    agent.shutdown().await.unwrap();
    let requests: Vec<Value> = fs::read_to_string(fixture.root.join("requests"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        requests
            .iter()
            .map(|request| request["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["thread/resume", "thread/resume", "turn/start"]
    );
}

#[tokio::test]
async fn mismatched_resume_response_never_starts_a_turn() {
    let fixture = Fixture::new(&format!(
        "{INITIALIZE}\n\
         IFS= read -r request || exit 1\n\
         printf '%s\\n' \"$request\" > requests\n\
         printf '%s\\n' '{{\"id\":2,\"result\":{{\"thread\":{{\"id\":\"other\"}}}}}}'\n\
         while IFS= read -r request; do printf '%s\\n' \"$request\" >> requests; done"
    ));
    let mut agent = fixture.agent().await;
    let error = agent.start_turn("stored", "prompt").await.unwrap_err();
    assert!(matches!(
        error.downcast_ref::<AppServerError>(),
        Some(AppServerError::InvalidResponse { method, .. }) if method == "thread/resume"
    ));
    assert!(agent.is_connection_fatal(&error));
    agent.shutdown().await.unwrap();
    let requests: Vec<Value> = fs::read_to_string(fixture.root.join("requests"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["method"], "thread/resume");
}
