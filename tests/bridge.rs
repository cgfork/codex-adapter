#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use codebridge_core::{Bridge, Memory, Store};
use codexbridge::{AppServer, Codex, CodexAgent};
use serde_json::{Value, json};
use teloxide::{
    Bot,
    types::{ChatId, UserId},
};
use tgbot::TelegramBot;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::timeout,
};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "codebridge-integration-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let executable = root.join("codex");
        fs::write(&executable, r##"#!/usr/bin/python3
import json, os, sys
from pathlib import Path
Path('pid').write_text(str(os.getpid()))
def send(value):
    print(json.dumps(value), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    if method == 'initialize':
        send({'id': request['id'], 'result': {}})
    elif method == 'thread/start':
        send({'id': request['id'], 'result': {'thread': {'id': 'session-1'}}})
    elif method == 'turn/start':
        with open('prompts', 'a') as prompts:
            prompts.write(request['params']['input'][0]['text'] + '\n')
        send({'id': request['id'], 'result': {'turn': {'id': 'turn-1'}}})
        send({'id': 'approval-1', 'method': 'item/commandExecution/requestApproval', 'params': {
            'threadId': 'session-1', 'turnId': 'turn-1', 'command': 'echo local', 'reason': 'Test approval'}})
    elif method is None and request.get('id') == 'approval-1':
        Path('decision').write_text(request['result']['decision'])
        if request['result']['decision'] == 'accept':
            send({'method': 'item/completed', 'params': {'threadId': 'session-1', 'turnId': 'turn-1',
                'item': {'type': 'agentMessage', 'text': '**Approved result**'}}})
            send({'method': 'turn/completed', 'params': {'threadId': 'session-1',
                'turn': {'id': 'turn-1', 'status': 'completed'}}})
"##).unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        Self(root)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(self.0.join("pid")) {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", &pid])
                .stderr(Stdio::null())
                .status();
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn message(id: i32, sender: u64, text: &str) -> Value {
    json!({"message_id": id, "date": 1_700_000_000,
        "chat": {"id": 42, "type": "private"},
        "from": {"id": sender, "is_bot": false, "first_name": "User"}, "text": text})
}

async fn request(stream: &mut TcpStream) -> (String, Value) {
    let mut data = Vec::new();
    let headers_end = loop {
        if let Some(index) = data.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break index + 4;
        }
        let mut buffer = [0; 2048];
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(count > 0, "request closed before headers");
        data.extend_from_slice(&buffer[..count]);
    };
    let headers = String::from_utf8(data[..headers_end].to_vec()).unwrap();
    let path = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap_or(0);
    while data.len() < headers_end + length {
        let mut buffer = [0; 2048];
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(count > 0, "request closed before body");
        data.extend_from_slice(&buffer[..count]);
    }
    let body = if length == 0 {
        json!({})
    } else {
        serde_json::from_slice(&data[headers_end..headers_end + length]).unwrap()
    };
    (path, body)
}

#[tokio::test]
async fn telegram_and_codex_share_the_core_session_and_approval_workflow() {
    let fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let (completed_tx, completed_rx) = oneshot::channel();
    let mock_api = tokio::spawn(async move {
        let mut completed_tx = Some(completed_tx);
        let mut stage = 0;
        let mut approval_visible = false;
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (path, body) = request(&mut stream).await;
            let method = path.rsplit('/').next().unwrap().to_ascii_lowercase();
            let result = match method.as_str() {
                "getme" => {
                    json!({"id": 123, "is_bot": true, "first_name": "Test", "username": "codebridge_bot", "can_join_groups": true, "can_read_all_group_messages": false, "supports_inline_queries": false, "has_main_web_app": false})
                }
                "setmycommands" | "answercallbackquery" | "deletewebhook" => json!(true),
                "getwebhookinfo" => {
                    json!({"url": "", "has_custom_certificate": false, "pending_update_count": 0})
                }
                "editmessagereplymarkup" => message(90, 123, "Approve?"),
                "getupdates" if stage == 0 => {
                    stage = 1;
                    json!([
                        {"update_id": 101, "message": message(1, 8, "unauthorized prompt")},
                        {"update_id": 102, "message": message(2, 7, "authorized prompt")},
                    ])
                }
                "getupdates" if stage == 1 && approval_visible => {
                    assert_eq!(body["offset"], 103);
                    stage = 2;
                    json!([
                        {"update_id": 103, "callback_query": {"id": "wrong-user", "chat_instance": "chat", "from": {"id": 8, "is_bot": false, "first_name": "Other"}, "message": message(90, 123, "Approve?"), "data": "allow"}},
                        {"update_id": 104, "callback_query": {"id": "owner", "chat_instance": "chat", "from": {"id": 7, "is_bot": false, "first_name": "Owner"}, "message": message(90, 123, "Approve?"), "data": "allow"}},
                    ])
                }
                "getupdates" => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    json!([])
                }
                "sendmessage" => {
                    let text = body["text"].as_str().unwrap();
                    if body.get("reply_markup").is_some() {
                        assert!(text.contains("Test approval"));
                        approval_visible = true;
                    }
                    if text.contains("<b>Approved result</b>") {
                        assert_eq!(body["parse_mode"], "HTML");
                        if let Some(tx) = completed_tx.take() {
                            let _ = tx.send(());
                        }
                    }
                    message(90, 123, text)
                }
                other => panic!("unexpected Telegram method: {other}"),
            };
            let response = json!({"ok": true, "result": result}).to_string();
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            // Shutdown may cancel a delivery after the mock has accepted it.
            if stream.write_all(headers.as_bytes()).await.is_ok() {
                let _ = stream.write_all(response.as_bytes()).await;
            }
        }
    });
    let codex = Codex::new(&fixture.0).with_executable(fixture.0.join("codex"));
    let server = AppServer::start(&codex).await.unwrap();
    let bot = Bot::new("123:local-integration-test").set_api_url(url.parse().unwrap());
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let sessions = Memory::default();
    let bridge = Bridge::new(TelegramBot::new(bot), CodexAgent::new(codex, server))
        .with_store(sessions.clone())
        .with_allowed_user_id(Some(UserId(7)));
    let mut running = tokio::spawn(async move {
        bridge
            .run(async { shutdown_rx.await.map_err(std::io::Error::other) })
            .await
    });
    let result = timeout(Duration::from_secs(10), completed_rx).await;
    let _ = shutdown_tx.send(());
    let stopped = timeout(Duration::from_secs(5), &mut running).await;
    if stopped.is_err() {
        running.abort();
        let _ = running.await;
    }
    mock_api.abort();
    let api_result = mock_api.await;
    assert!(
        api_result.is_err_and(|error| error.is_cancelled()),
        "mock Telegram API panicked"
    );
    assert!(
        result.is_ok(),
        "bridge never delivered the approved result; stopped: {stopped:?}; decision: {:?}",
        fs::read_to_string(fixture.0.join("decision"))
    );
    result.unwrap().unwrap();
    stopped.unwrap().unwrap().unwrap();
    assert_eq!(
        fs::read_to_string(fixture.0.join("prompts")).unwrap(),
        "authorized prompt\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.0.join("decision")).unwrap(),
        "accept"
    );
    assert_eq!(
        sessions
            .load(&ChatId(42))
            .await
            .unwrap()
            .unwrap()
            .session_ids,
        ["session-1"]
    );
    let pid = fs::read_to_string(fixture.0.join("pid")).unwrap();
    assert!(
        !Command::new("/bin/kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
}
