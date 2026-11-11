use std::{collections::HashMap, future::Future, io};

use codexbridge::{AppServer, AppServerError, BridgeResult, Codex, ExecOutput};
use serde_json::{Value, json};
use teloxide::types::{CallbackQuery, ChatId, Message, UserId};
use tgbot::{ApprovalSignal, Command, IncomingMessage};
use tokio::{
    sync::mpsc,
    task::{AbortHandle, JoinHandle, JoinSet},
};

use crate::{
    access,
    outbox::{ApprovalDelivery, Outbox},
    sessions::{ActiveTurn, Sessions},
};

pub(crate) async fn run(
    codex: Codex,
    proxy: Option<&str>,
    allowed_user_id: Option<UserId>,
    shutdown: impl Future<Output = io::Result<()>>,
) -> BridgeResult<()> {
    let token = std::env::var("TELOXIDE_TOKEN")
        .ok()
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| {
            io::Error::other("bridge requires a nonempty TELOXIDE_TOKEN environment variable")
        })?;
    let bot = tgbot::create_bot(token, proxy)?;
    tracing::info!(
        allowed_user_id = allowed_user_id.map(|id| id.0),
        "Starting Telegram bridge"
    );
    tokio::pin!(shutdown);
    let Some(server) = AppServer::start_with_shutdown(&codex, shutdown.as_mut())
        .await
        .map_err(|error| io::Error::other(format!("failed to start Codex app-server: {error}")))?
    else {
        tracing::info!("Bridge startup cancelled");
        return Ok(());
    };
    let (sender, receiver) = mpsc::channel(64);
    let (approval_sender, approval_deliveries) = mpsc::unbounded_channel();
    let telegram_bot = bot.clone();
    let mut telegram = tokio::spawn(async move { tgbot::run(telegram_bot, sender).await });
    let mut runtime = Runtime {
        allowed_user_id,
        codex,
        server,
        outbox: Outbox::new(bot, approval_sender),
        approval_deliveries,
        sessions: Sessions::default(),
        exec_jobs: JoinSet::new(),
        running_execs: HashMap::new(),
    };
    let result = runtime.dispatch(receiver, &mut telegram, shutdown).await;
    tracing::info!(
        sessions = runtime.sessions.chats.len(),
        running_tasks = runtime.running_execs.len(),
        pending_approvals = runtime.sessions.approvals.len(),
        "Stopping Telegram bridge"
    );
    telegram.abort();
    runtime.exec_jobs.abort_all();
    runtime.sessions.approvals.clear();
    let (_, shutdown) = tokio::join!(runtime.exec_jobs.shutdown(), runtime.server.shutdown());
    runtime.running_execs.clear();
    if !telegram.is_finished() {
        let _ = telegram.await;
    }
    if let Err(error) = &result {
        let chats: Vec<_> = runtime.sessions.chats.keys().copied().collect();
        for chat_id in chats {
            runtime.notice(
                chat_id,
                &format!("Bridge stopped: {error}. Restart the adapter to reconnect."),
            );
        }
    }
    runtime.outbox.shutdown().await;
    let result = result.and(shutdown);
    tracing::info!(success = result.is_ok(), "Telegram bridge stopped");
    result
}

struct Runtime {
    allowed_user_id: Option<UserId>,
    codex: Codex,
    server: AppServer,
    outbox: Outbox,
    approval_deliveries: mpsc::UnboundedReceiver<ApprovalDelivery>,
    sessions: Sessions,
    exec_jobs: JoinSet<(ChatId, BridgeResult<ExecOutput>)>,
    running_execs: HashMap<ChatId, AbortHandle>,
}

impl Runtime {
    async fn dispatch(
        &mut self,
        incoming: mpsc::Receiver<IncomingMessage>,
        telegram: &mut JoinHandle<Result<(), teloxide::RequestError>>,
        shutdown: impl Future<Output = io::Result<()>>,
    ) -> BridgeResult<()> {
        // Select around the whole loop so shutdown also cancels an awaited RPC.
        tokio::select! {
            biased;
            result = shutdown => result.map_err(Into::into),
            result = self.dispatch_events(incoming, telegram) => result,
        }
    }

    async fn dispatch_events(
        &mut self,
        mut incoming: mpsc::Receiver<IncomingMessage>,
        telegram: &mut JoinHandle<Result<(), teloxide::RequestError>>,
    ) -> BridgeResult<()> {
        loop {
            tokio::select! {
                event = self.server.next_event() => {
                    let event = event?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "Codex app-server closed stdout"))?;
                    self.server_event(event).await?;
                }
                result = &mut *telegram => {
                    result??;
                    return Ok(());
                }
                event = incoming.recv() => {
                    let Some(event) = event else { return Ok(()); };
                    self.incoming(event).await?;
                }
                delivery = self.approval_deliveries.recv() => {
                    if let Some(delivery) = delivery {
                        self.approval_delivered(delivery).await?;
                    }
                }
                completion = self.exec_jobs.join_next_with_id(), if !self.exec_jobs.is_empty() => {
                    match completion {
                        Some(Ok((id, (chat_id, result)))) => {
                            if self.running_execs.get(&chat_id).is_some_and(|handle| handle.id() == id) {
                                self.running_execs.remove(&chat_id);
                                self.exec_completed(chat_id, result);
                            }
                        }
                        Some(Err(error)) if !error.is_cancelled() => {
                            let chat_id = self.running_execs.iter().find_map(|(chat_id, handle)| {
                                (handle.id() == error.id()).then_some(*chat_id)
                            });
                            if let Some(chat_id) = chat_id {
                                self.running_execs.remove(&chat_id);
                                tracing::error!(chat_id = chat_id.0, %error, "One-off task panicked");
                                self.notice(chat_id, &format!("One-off task failed: {error}"));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn incoming(&mut self, incoming: IncomingMessage) -> BridgeResult<()> {
        if !access::is_allowed(self.allowed_user_id, &incoming) {
            tracing::debug!("Ignoring Telegram update from unauthorized sender");
            return Ok(());
        }
        let (command, message) = match incoming {
            IncomingMessage::ApprovalSignal { signal, query } => {
                return self.approval(signal, &query).await;
            }
            IncomingMessage::Command { command, message } => (Some(command), message),
            IncomingMessage::Raw(message) => (None, message),
        };
        let chat_id = message.chat.id;
        let result = match command {
            Some(Command::Sessions) => {
                self.list_sessions(chat_id);
                Ok(())
            }
            Some(Command::Init) => self.init(chat_id, false).await.map(|_| ()),
            Some(Command::New) => self.init(chat_id, true).await.map(|_| ()),
            Some(Command::Delete) => self.delete(chat_id),
            Some(Command::Interrupt) => self.interrupt(chat_id).await,
            Some(Command::Exec(prompt)) => self.exec(chat_id, prompt),
            None => {
                match message.text() {
                    Some(text) if text.starts_with('/') => {
                        self.notice(chat_id, "Unknown command. Use /init, /new, /delete, /sessions, /interrupt, or /exec <prompt>.");
                        Ok(())
                    }
                    Some(text) => self.start_turn(&message, text).await,
                    None => {
                        self.notice(chat_id, "Send a text prompt or use /exec <prompt>; media input is not supported.");
                        Ok(())
                    }
                }
            }
        };
        if let Err(error) = result {
            tracing::warn!(chat_id = chat_id.0, %error, "Chat request failed");
            if error
                .downcast_ref::<AppServerError>()
                .is_some_and(AppServerError::is_connection_fatal)
            {
                return Err(error);
            }
            self.notice(chat_id, &format!("Request failed: {error}"));
        }
        Ok(())
    }

    async fn init(&mut self, chat_id: ChatId, new: bool) -> BridgeResult<String> {
        if let Some(session) = self.sessions.chats.get(&chat_id) {
            if !new {
                if let Some(thread_id) = session.active_thread() {
                    let thread_id = thread_id.to_owned();
                    self.notice(chat_id, &format!("Active session: {thread_id}"));
                    return Ok(thread_id);
                }
            }
            if session.turn.is_some() {
                return Err(io::Error::other(
                    "a turn is running; use /interrupt and wait for completion before /new",
                )
                .into());
            }
        }
        let thread_id = self.server.start_thread(&self.codex).await?;
        self.sessions
            .chats
            .entry(chat_id)
            .or_default()
            .threads
            .push(thread_id.clone());
        tracing::info!(chat_id = chat_id.0, %thread_id, "Started session");
        self.notice(chat_id, &format!("Started session: {thread_id}"));
        Ok(thread_id)
    }

    fn delete(&mut self, chat_id: ChatId) -> BridgeResult<()> {
        let Some(thread_id) = self.sessions.delete_current(chat_id)? else {
            self.notice(
                chat_id,
                "No active session to delete. Use /init or send a text prompt.",
            );
            return Ok(());
        };
        let next = match self
            .sessions
            .chats
            .get(&chat_id)
            .and_then(|session| session.active_thread())
        {
            Some(active) => format!("Active session: {active}"),
            None => "No sessions left. Use /init or send a text prompt.".to_owned(),
        };
        tracing::info!(chat_id = chat_id.0, %thread_id, "Deleted session");
        self.notice(chat_id, &format!("Deleted session: {thread_id}\n{next}"));
        Ok(())
    }

    fn list_sessions(&mut self, chat_id: ChatId) {
        let text = match self.sessions.chats.get(&chat_id) {
            Some(session) => session
                .threads
                .iter()
                .map(|thread| {
                    let marker = if session.active_thread() == Some(thread.as_str()) {
                        " (active)"
                    } else {
                        ""
                    };
                    format!("{thread}{marker}")
                })
                .collect::<Vec<_>>()
                .join("\n"),
            None => "No sessions yet. Use /init or send a text prompt.".to_owned(),
        };
        self.notice(chat_id, &text);
    }

    async fn start_turn(&mut self, message: &Message, prompt: &str) -> BridgeResult<()> {
        if prompt.trim().is_empty() {
            return Err(io::Error::other("prompt must not be blank").into());
        }
        let chat_id = message.chat.id;
        if self
            .sessions
            .chats
            .get(&chat_id)
            .is_some_and(|session| session.turn.is_some())
        {
            return Err(
                io::Error::other("a turn is already running; wait or use /interrupt").into(),
            );
        }
        let thread_id = match self
            .sessions
            .chats
            .get(&chat_id)
            .and_then(|session| session.active_thread())
        {
            Some(thread_id) => thread_id.to_owned(),
            None => self.init(chat_id, false).await?,
        };
        let turn_id = self.server.start_turn(&thread_id, prompt).await?;
        tracing::info!(chat_id = chat_id.0, %thread_id, %turn_id, "Started turn");
        self.sessions.chats.entry(chat_id).or_default().turn = Some(ActiveTurn {
            id: turn_id,
            owner: message.from.as_ref().map(|user| user.id),
        });
        Ok(())
    }

    async fn interrupt(&mut self, chat_id: ChatId) -> BridgeResult<()> {
        let mut interrupted = false;
        if let Some(handle) = self.running_execs.remove(&chat_id) {
            handle.abort();
            tracing::info!(chat_id = chat_id.0, "Cancelled one-off task");
            interrupted = true;
        }
        if let Some((thread_id, turn_id)) = self.sessions.chats.get(&chat_id).and_then(|session| {
            Some((
                session.active_thread()?.to_owned(),
                session.turn.as_ref()?.id.clone(),
            ))
        }) {
            let tickets: Vec<_> = self
                .sessions
                .approvals
                .iter()
                .filter(|(_, pending)| pending.chat_id == chat_id)
                .map(|(ticket, _)| *ticket)
                .collect();
            for ticket in tickets {
                if let Some(pending) = self.sessions.approvals.remove(&ticket) {
                    let request_id = pending.request_id.clone();
                    if let Some(message_id) = pending.message_id {
                        self.outbox.clear_approval(chat_id, message_id);
                    }
                    drop(pending);
                    self.server
                        .respond(request_id, json!({"decision":"decline"}))
                        .await?;
                }
            }
            self.server.interrupt(&thread_id, &turn_id).await?;
            tracing::info!(chat_id = chat_id.0, %thread_id, %turn_id, "Requested turn interruption");
            interrupted = true;
        }
        self.notice(
            chat_id,
            if interrupted {
                "Interrupt requested."
            } else {
                "No operation is running."
            },
        );
        Ok(())
    }

    fn exec(&mut self, chat_id: ChatId, prompt: String) -> BridgeResult<()> {
        if prompt.trim().is_empty() {
            return Err(io::Error::other("usage: /exec <nonblank prompt>").into());
        }
        if self.running_execs.contains_key(&chat_id) {
            return Err(io::Error::other(
                "a one-off task is already running; wait or use /interrupt",
            )
            .into());
        }
        let codex = self.codex.clone();
        let handle = self
            .exec_jobs
            .spawn(async move { (chat_id, codex.exec(&prompt).await) });
        self.running_execs.insert(chat_id, handle);
        tracing::info!(chat_id = chat_id.0, "Started one-off task");
        self.notice(chat_id, "Started one-off Codex task.");
        Ok(())
    }

    fn exec_completed(&mut self, chat_id: ChatId, result: BridgeResult<ExecOutput>) {
        match result {
            Err(error) => {
                tracing::error!(chat_id = chat_id.0, %error, "One-off task failed");
                self.notice(chat_id, &format!("One-off task failed: {error}"));
            }
            Ok(output) => {
                tracing::info!(chat_id = chat_id.0, status = %output.status, "One-off task completed");
                let empty = output.stdout.trim().is_empty();
                if !empty {
                    self.outbox.output(chat_id, output.stdout);
                }
                if !output.status.success() {
                    let detail = if output.stderr.trim().is_empty() {
                        format!("Codex exited with {}.", output.status)
                    } else {
                        format!("Codex exited with {}.\n{}", output.status, output.stderr)
                    };
                    self.notice(chat_id, &detail);
                } else if empty {
                    self.notice(chat_id, "One-off task completed without output.");
                }
            }
        }
    }

    async fn server_event(&mut self, event: Value) -> BridgeResult<()> {
        let method = event["method"].as_str().unwrap_or_default();
        if event.get("id").is_some() && !method.is_empty() {
            return self.server_request(event).await;
        }
        if method == "serverRequest/resolved" {
            self.sessions.resolve_request(&event);
            return Ok(());
        }
        if matches!(method, "item/completed" | "error" | "turn/completed") {
            if let Some(chat_id) = self.sessions.route(&event) {
                self.outbox.output(chat_id, event.to_string());
            }
        }
        if let Some(chat_id) = self.sessions.finish_turn(&event) {
            let status = event["params"]["turn"]["status"]
                .as_str()
                .unwrap_or_default();
            tracing::info!(
                chat_id = chat_id.0,
                thread_id = event["params"]["threadId"].as_str().unwrap_or_default(),
                turn_id = event["params"]["turn"]["id"].as_str().unwrap_or_default(),
                status,
                "Turn completed"
            );
            if status == "interrupted" {
                self.notice(chat_id, "Turn interrupted.");
            } else if status == "failed"
                && event["params"]["turn"]["error"]["message"]
                    .as_str()
                    .is_none()
            {
                self.notice(chat_id, "Codex turn failed without an error message.");
            }
        }
        Ok(())
    }

    async fn server_request(&mut self, event: Value) -> BridgeResult<()> {
        let method = event["method"].as_str().unwrap_or_default();
        let request_id = event["id"].clone();
        if !matches!(
            method,
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
        ) {
            self.server.reject_request(request_id).await?;
            tracing::warn!(method, "Rejected unsupported app-server request");
            if let Some(chat_id) = event["params"]["threadId"]
                .as_str()
                .and_then(|thread| self.sessions.chat_for_thread(thread))
            {
                self.notice(chat_id, &format!("Codex requested unsupported interaction: {method}. The request was rejected."));
            }
            return Ok(());
        }
        let Some(chat_id) = self.sessions.route(&event) else {
            return self
                .server
                .respond(request_id, json!({"decision":"decline"}))
                .await;
        };
        let params = &event["params"];
        let turn = self.sessions.chats[&chat_id]
            .turn
            .as_ref()
            .expect("routed event has an active turn");
        let Some(owner) = turn
            .owner
            .filter(|_| params["turnId"].as_str() == Some(turn.id.as_str()))
        else {
            self.server
                .respond(request_id, json!({"decision":"decline"}))
                .await?;
            self.notice(chat_id, "Approval declined: the request could not be associated with its initiating user and turn.");
            return Ok(());
        };
        let turn_id = turn.id.clone();
        let thread_id = params["threadId"].as_str().unwrap().to_owned();
        let ticket = self
            .sessions
            .queue_approval(chat_id, request_id, thread_id, turn_id, owner);
        tracing::debug!(
            chat_id = chat_id.0,
            ticket,
            method,
            "Queued approval request"
        );
        self.outbox.approval(
            chat_id,
            ticket,
            approval_description(method, params),
            self.sessions.approvals[&ticket].live.clone(),
        );
        Ok(())
    }

    async fn approval_delivered(&mut self, delivery: ApprovalDelivery) -> BridgeResult<()> {
        match delivery.result {
            Ok(message_id) => {
                if !self.sessions.register_delivery(delivery.ticket, message_id) {
                    self.sessions.approvals.remove(&delivery.ticket);
                    self.outbox.clear_approval(delivery.chat_id, message_id);
                }
            }
            Err(error) => {
                tracing::warn!(chat_id = delivery.chat_id.0, ticket = delivery.ticket, %error, "Could not deliver approval prompt");
                if let Some(pending) = self.sessions.approvals.remove(&delivery.ticket) {
                    let current = self.sessions.pending_is_current(&pending);
                    let request_id = pending.request_id.clone();
                    drop(pending);
                    if current {
                        self.server
                            .respond(request_id, json!({"decision":"decline"}))
                            .await?;
                        self.notice(
                            delivery.chat_id,
                            "Approval prompt could not be delivered; the request was declined.",
                        );
                    }
                }
            }
        }
        Ok(())
    }

    async fn approval(
        &mut self,
        signal: ApprovalSignal,
        query: &CallbackQuery,
    ) -> BridgeResult<()> {
        let Some(message) = query.regular_message() else {
            return Ok(());
        };
        let chat_id = message.chat.id;
        let Some(ticket) = self
            .sessions
            .approval_ticket(chat_id, message.id, query.from.id)
        else {
            self.notice(
                chat_id,
                "This approval has expired or belongs to another user.",
            );
            return Ok(());
        };
        let pending = self.sessions.approvals.remove(&ticket).unwrap();
        let request_id = pending.request_id.clone();
        drop(pending);
        self.outbox.clear_approval(chat_id, message.id);
        let decision = match signal {
            ApprovalSignal::Allow => "accept",
            ApprovalSignal::Deny => "decline",
        };
        tracing::info!(
            chat_id = chat_id.0,
            ticket,
            decision,
            "Answered approval request"
        );
        self.server
            .respond(request_id, json!({"decision":decision}))
            .await
    }

    fn notice(&mut self, chat_id: ChatId, text: &str) {
        self.outbox.notice(chat_id, text);
    }
}

fn approval_description(method: &str, params: &Value) -> String {
    let kind = if method == "item/commandExecution/requestApproval" {
        "command execution"
    } else {
        "file changes"
    };
    let reason = params["reason"]
        .as_str()
        .unwrap_or("Codex requires your approval.");
    let command = params["command"].as_str().unwrap_or_default();
    let cwd = params["cwd"].as_str().unwrap_or_default();
    let grant_root = params["grantRoot"].as_str().unwrap_or_default();
    let text = format!(
        "Approve {kind}?\n{reason}\n{command}\n{cwd}\n{grant_root}\nOnly the user who started this turn can decide."
    );
    text.chars().take(1800).collect()
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use teloxide::Bot;
    use tokio::time::{sleep, timeout};

    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "adapter-rpc-shutdown-{}-{timestamp}",
                std::process::id()
            ));
            fs::create_dir(&root).unwrap();
            fs::write(
                root.join("codex"),
                r#"#!/bin/sh
IFS= read -r initialize || exit 1
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r initialized || exit 1
IFS= read -r request || exit 1
printf '%s\n' "$request" > request
while IFS= read -r request; do :; done
"#,
            )
            .unwrap();
            fs::set_permissions(root.join("codex"), fs::Permissions::from_mode(0o700)).unwrap();
            Self(root)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn unauthorized_updates_cannot_run_tasks_mutate_sessions_or_queue_replies() {
        let fixture = Fixture::new();
        let codex = Codex::new(&fixture.0).with_executable(fixture.0.join("codex"));
        let server = AppServer::start(&codex).await.unwrap();
        let (approval_sender, approval_deliveries) = mpsc::unbounded_channel();
        let bot = Bot::new("test-token").set_api_url("http://127.0.0.1:9".parse().unwrap());
        let mut runtime = Runtime {
            allowed_user_id: Some(UserId(123)),
            codex,
            server,
            outbox: Outbox::new(bot, approval_sender),
            approval_deliveries,
            sessions: Sessions::default(),
            exec_jobs: JoinSet::new(),
            running_execs: HashMap::new(),
        };
        let chat_id = ChatId(123);
        runtime
            .sessions
            .chats
            .entry(chat_id)
            .or_default()
            .threads
            .push("existing-thread".into());
        for text in [
            "hello",
            "/init",
            "/new",
            "/delete",
            "/sessions",
            "/interrupt",
            "/exec task",
            "/unknown",
        ] {
            let message = serde_json::from_value(json!({
                "message_id": 1,
                "date": 1_700_000_000,
                "chat": {"id": chat_id.0, "type": "private", "first_name": "Owner"},
                "from": {"id": 7, "is_bot": false, "first_name": "Other"},
                "text": text
            }))
            .unwrap();
            timeout(
                Duration::from_secs(1),
                runtime.incoming(IncomingMessage::from_message(message, "adapter_bot")),
            )
            .await
            .expect("unauthorized messages must not reach Codex RPCs")
            .unwrap();
            assert_eq!(
                runtime.sessions.chats[&chat_id].threads,
                ["existing-thread"]
            );
            assert!(runtime.sessions.chats[&chat_id].turn.is_none());
            assert!(runtime.running_execs.is_empty());
            assert!(runtime.exec_jobs.is_empty());
            assert!(runtime.outbox.is_idle());
        }

        // Even a valid turn owner must satisfy the configured account filter.
        runtime.sessions.chats.get_mut(&chat_id).unwrap().turn = Some(ActiveTurn {
            id: "existing-turn".into(),
            owner: Some(UserId(7)),
        });
        let ticket = runtime.sessions.queue_approval(
            chat_id,
            json!(42),
            "existing-thread".into(),
            "existing-turn".into(),
            UserId(7),
        );
        assert!(
            runtime
                .sessions
                .register_delivery(ticket, teloxide::types::MessageId(10))
        );
        for data in ["allow", "deny"] {
            let query = serde_json::from_value(json!({
                "id": "query",
                "from": {"id": 7, "is_bot": false, "first_name": "Other"},
                "chat_instance": "instance",
                "message": {
                    "message_id": 10,
                    "date": 1_700_000_000,
                    "chat": {"id": chat_id.0, "type": "private", "first_name": "Owner"},
                    "text": "Approve?"
                },
                "data": data
            }))
            .unwrap();
            runtime
                .incoming(IncomingMessage::from_callback_query(query).unwrap())
                .await
                .unwrap();
            assert!(runtime.sessions.approvals.contains_key(&ticket));
            assert!(runtime.outbox.is_idle());
        }
        runtime.server.shutdown().await.unwrap();
        assert!(!fixture.0.join("request").exists());
    }

    #[tokio::test]
    async fn shutdown_cancels_an_in_flight_rpc_without_waiting_for_its_timeout() {
        let fixture = Fixture::new();
        let codex = Codex::new(&fixture.0).with_executable(fixture.0.join("codex"));
        let server = AppServer::start(&codex).await.unwrap();
        let (approval_sender, approval_deliveries) = mpsc::unbounded_channel();
        let mut runtime = Runtime {
            allowed_user_id: None,
            codex,
            server,
            outbox: Outbox::new(Bot::new("test-token"), approval_sender),
            approval_deliveries,
            sessions: Sessions::default(),
            exec_jobs: JoinSet::new(),
            running_execs: HashMap::new(),
        };
        let (sender, receiver) = mpsc::channel(1);
        let message = serde_json::from_value(json!({
            "message_id": 1,
            "date": 1_700_000_000,
            "chat": {"id": 1, "type": "private", "first_name": "Test"},
            "text": "/init"
        }))
        .unwrap();
        sender
            .send(IncomingMessage::from_message(message, "adapter_bot"))
            .await
            .unwrap();
        let mut telegram = tokio::spawn(std::future::pending());
        let shutdown = async {
            while !fixture.0.join("request").exists() {
                sleep(Duration::from_millis(5)).await;
            }
            Ok(())
        };

        let result = timeout(
            Duration::from_secs(3),
            runtime.dispatch(receiver, &mut telegram, shutdown),
        )
        .await;
        telegram.abort();
        let _ = telegram.await;
        runtime.server.shutdown().await.unwrap();

        result
            .expect("shutdown should cancel the blocked RPC")
            .unwrap();
        let request: Value =
            serde_json::from_slice(&fs::read(fixture.0.join("request")).unwrap()).unwrap();
        assert_eq!(request["method"], "thread/start");
        assert!(runtime.sessions.chats.is_empty());
    }
}
