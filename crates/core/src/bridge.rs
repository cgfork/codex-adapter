use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    io,
};

use tokio::{
    sync::mpsc,
    task::{AbortHandle, JoinHandle, JoinSet},
};

use crate::{
    AgentEvent, ApprovalDecision, Bot, BotEvent, BridgeResult, CodeAgent, Command, ExecutionOutput,
    IncomingMessage, Memory, Session, SessionState, Store, TurnStatus,
    outbox::{ApprovalDelivery, Outbox},
    sessions::{ActiveTurn, ChatSession, Sessions},
};

/// Session and approval workflow shared by any bot and code-agent pair.
pub struct Bridge<B: Bot, A: CodeAgent, S: Store<ChatId = B::ChatId> = Memory<<B as Bot>::ChatId>> {
    bot: B,
    agent: A,
    store: S,
    allowed_user_id: Option<B::UserId>,
}

#[derive(Debug)]
struct UserError(String);

impl fmt::Display for UserError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for UserError {}

fn user_error(message: impl Into<String>) -> crate::BridgeError {
    Box::new(UserError(message.into()))
}

#[derive(Debug)]
struct StoreError(crate::BridgeError);

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "session store failed: {}", self.0)
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

fn store_error(error: crate::BridgeError) -> crate::BridgeError {
    Box::new(StoreError(error))
}

struct BotTask(JoinHandle<BridgeResult<()>>);

impl Drop for BotTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<B: Bot, A: CodeAgent> Bridge<B, A, Memory<B::ChatId>> {
    /// Compose a bot with an already initialized agent connection.
    pub fn new(bot: B, agent: A) -> Self {
        Self {
            bot,
            agent,
            store: Memory::default(),
            allowed_user_id: None,
        }
    }
}

impl<B: Bot, A: CodeAgent, S: Store<ChatId = B::ChatId>> Bridge<B, A, S> {
    /// Replace the session history store while preserving the bot and agent.
    pub fn with_store<T: Store<ChatId = B::ChatId>>(self, store: T) -> Bridge<B, A, T> {
        Bridge {
            bot: self.bot,
            agent: self.agent,
            store,
            allowed_user_id: self.allowed_user_id,
        }
    }

    /// Restrict all incoming actions to one human user; `None` allows all users.
    /// Approval decisions still require the user who started the turn.
    pub fn with_allowed_user_id(mut self, user_id: Option<B::UserId>) -> Self {
        self.allowed_user_id = user_id;
        self
    }

    /// Run until cancellation, bot completion, or a fatal agent error.
    /// Signal the supplied future to stop gracefully and shut down the agent.
    pub async fn run(mut self, shutdown: impl Future<Output = io::Result<()>>) -> BridgeResult<()> {
        tokio::pin!(shutdown);
        tokio::select! {
            biased;
            result = &mut shutdown => {
                let stopped = self.agent.shutdown().await;
                return result.map_err(Into::into).and(stopped);
            }
            () = std::future::ready(()) => {}
        }
        let snapshot = tokio::select! {
            biased;
            result = &mut shutdown => {
                let stopped = self.agent.shutdown().await;
                return result.map_err(Into::into).and(stopped);
            }
            result = self.store.list() => match result {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    let stopped = self.agent.shutdown().await;
                    return Err::<(), _>(store_error(error)).and(stopped);
                }
            }
        };
        let mut sessions = Sessions::default();
        let mut seen_ids = HashSet::new();
        let mut seen_chats = HashSet::new();
        for (chat, state) in snapshot {
            if !seen_chats.insert(chat.clone()) {
                let stopped = self.agent.shutdown().await;
                return Err::<(), _>(store_error(
                    io::Error::other("duplicate chat in stored histories").into(),
                ))
                .and(stopped);
            }
            if state
                .session_ids()
                .iter()
                .any(|id| !seen_ids.insert(id.clone()))
            {
                let stopped = self.agent.shutdown().await;
                return Err::<(), _>(store_error(
                    io::Error::other("duplicate session ID in stored histories").into(),
                ))
                .and(stopped);
            }
            if !state.session_ids().is_empty() {
                sessions.chats.insert(
                    chat,
                    ChatSession {
                        threads: state.to_state().session_ids,
                        turn: None,
                    },
                );
            }
        }
        tracing::info!(sessions = sessions.chats.len(), "Loaded session histories");
        let (sender, receiver) = mpsc::channel(64);
        let (approval_sender, approval_deliveries) = mpsc::unbounded_channel();
        let bot = self.bot.clone();
        let mut bot_task = BotTask(tokio::spawn(async move { bot.run(sender).await }));
        let mut runtime = Runtime {
            agent: self.agent,
            store: self.store,
            allowed_user_id: self.allowed_user_id,
            sessions,
            outbox: Outbox::new(self.bot, approval_sender),
            approval_deliveries,
            exec_jobs: JoinSet::new(),
            running_execs: HashMap::new(),
        };
        let result = tokio::select! {
            biased;
            result = shutdown => result.map_err(Into::into),
            result = runtime.dispatch(receiver, &mut bot_task.0) => result,
        };
        tracing::info!(
            sessions = runtime.sessions.chats.len(),
            running_tasks = runtime.running_execs.len(),
            pending_approvals = runtime.sessions.approvals.len(),
            "Stopping bridge"
        );
        bot_task.0.abort();
        runtime.sessions.approvals.clear();
        let (_, agent_shutdown) =
            tokio::join!(runtime.exec_jobs.shutdown(), runtime.agent.shutdown());
        runtime.running_execs.clear();
        if !bot_task.0.is_finished() {
            let _ = (&mut bot_task.0).await;
        }
        if let Err(error) = &result {
            let chats: Vec<_> = runtime.sessions.chats.keys().cloned().collect();
            for chat in chats {
                runtime.outbox.notice(
                    chat,
                    format!("Bridge stopped: {error}. Restart to reconnect."),
                );
            }
        }
        runtime.outbox.shutdown().await;
        let result = result.and(agent_shutdown);
        tracing::info!(success = result.is_ok(), "Bridge stopped");
        result
    }
}

struct Runtime<B: Bot, A: CodeAgent, S: Store<ChatId = B::ChatId> = Memory<<B as Bot>::ChatId>> {
    agent: A,
    store: S,
    allowed_user_id: Option<B::UserId>,
    sessions: Sessions<B::ChatId, B::UserId, B::MessageId>,
    outbox: Outbox<B>,
    approval_deliveries: mpsc::UnboundedReceiver<ApprovalDelivery<B::ChatId, B::MessageId>>,
    exec_jobs: JoinSet<(B::ChatId, BridgeResult<ExecutionOutput>)>,
    running_execs: HashMap<B::ChatId, AbortHandle>,
}

impl<B: Bot, A: CodeAgent, S: Store<ChatId = B::ChatId>> Runtime<B, A, S> {
    async fn dispatch(
        &mut self,
        mut incoming: mpsc::Receiver<BotEvent<B>>,
        bot: &mut JoinHandle<BridgeResult<()>>,
    ) -> BridgeResult<()> {
        loop {
            tokio::select! {
                event = self.agent.next_event() => {
                    let event = event?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "agent event stream closed"))?;
                    self.agent_event(event).await?;
                }
                result = &mut *bot => { result??; return Ok(()); }
                event = incoming.recv() => {
                    let Some(event) = event else { return bot.await? };
                    self.incoming(event).await?;
                }
                delivery = self.approval_deliveries.recv() => {
                    if let Some(delivery) = delivery { self.approval_delivered(delivery).await?; }
                }
                completion = self.exec_jobs.join_next_with_id(), if !self.exec_jobs.is_empty() => {
                    match completion {
                        Some(Ok((id, (chat, result)))) => {
                            if self.running_execs.get(&chat).is_some_and(|handle| handle.id() == id) {
                                self.running_execs.remove(&chat);
                                self.exec_completed(chat, result);
                            }
                        }
                        Some(Err(error)) if !error.is_cancelled() => {
                            let chat = self.running_execs.iter().find_map(|(chat, handle)|
                                (handle.id() == error.id()).then(|| chat.clone()));
                            if let Some(chat) = chat {
                                self.running_execs.remove(&chat);
                                self.outbox.notice(chat, format!("One-off task failed: {error}"));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn incoming(&mut self, event: BotEvent<B>) -> BridgeResult<()> {
        let sender = match &event {
            IncomingMessage::Message { sender, .. } | IncomingMessage::Approval { sender, .. } => {
                sender
            }
        };
        if self
            .allowed_user_id
            .as_ref()
            .is_some_and(|allowed| sender.as_ref() != Some(allowed))
        {
            return Ok(());
        }
        match event {
            IncomingMessage::Approval {
                chat_id,
                message_id,
                sender,
                decision,
            } => self.approval(chat_id, message_id, sender, decision).await,
            IncomingMessage::Message {
                chat_id,
                sender,
                command,
                text,
            } => {
                let result = match command {
                    Some(Command::Sessions) => {
                        self.list_sessions(chat_id.clone());
                        Ok(())
                    }
                    Some(Command::Init) => self.init(chat_id.clone(), false).await.map(|_| ()),
                    Some(Command::New) => self.init(chat_id.clone(), true).await.map(|_| ()),
                    Some(Command::Delete) => self.delete(chat_id.clone()).await,
                    Some(Command::Interrupt) => self.interrupt(chat_id.clone()).await,
                    Some(Command::Exec(prompt)) => self.exec(chat_id.clone(), prompt),
                    None => match text {
                        Some(text) if text.starts_with('/') => {
                            self.outbox.notice(chat_id.clone(), "Unknown command. Use /init, /new, /delete, /sessions, /interrupt, or /exec <prompt>.");
                            Ok(())
                        }
                        Some(text) => self.start_turn(chat_id.clone(), sender, &text).await,
                        None => {
                            self.outbox.notice(chat_id.clone(), "Send a text prompt or use /exec <prompt>; media input is not supported.");
                            Ok(())
                        }
                    },
                };
                if let Err(error) = result {
                    tracing::warn!(chat_id = ?chat_id, %error, "Chat request failed");
                    if error.downcast_ref::<StoreError>().is_some()
                        || (error.downcast_ref::<UserError>().is_none()
                            && self.agent.is_connection_fatal(&error))
                    {
                        return Err(error);
                    }
                    self.outbox
                        .notice(chat_id, format!("Request failed: {error}"));
                }
                Ok(())
            }
        }
    }

    async fn init(&mut self, chat: B::ChatId, new: bool) -> BridgeResult<String> {
        if let Some(session) = self.sessions.chats.get(&chat) {
            if !new {
                if let Some(id) = session.active_thread() {
                    let id = id.to_owned();
                    self.outbox.notice(chat, format!("Active session: {id}"));
                    return Ok(id);
                }
            }
            if session.turn.is_some() {
                return Err(user_error(
                    "a turn is running; use /interrupt and wait for completion before /new",
                ));
            }
        }
        let id = self.agent.start_session().await?;
        let mut history = self
            .sessions
            .chats
            .get(&chat)
            .map_or_else(Vec::new, |s| s.threads.clone());
        history.push(id.clone());
        self.store
            .save(
                chat.clone(),
                SessionState {
                    session_ids: history,
                },
            )
            .await
            .map_err(store_error)?;
        self.sessions
            .chats
            .entry(chat.clone())
            .or_default()
            .threads
            .push(id.clone());
        tracing::info!(chat_id = ?chat, session_id = %id, "Started session");
        self.outbox.notice(chat, format!("Started session: {id}"));
        Ok(id)
    }

    async fn delete(&mut self, chat: B::ChatId) -> BridgeResult<()> {
        let Some(session) = self.sessions.chats.get(&chat) else {
            self.outbox.notice(
                chat,
                "No active session to delete. Use /init or send a text prompt.",
            );
            return Ok(());
        };
        if session.turn.is_some() {
            return Err(user_error(
                "a turn is running; use /interrupt and wait for completion before /delete",
            ));
        }
        let Some(id) = session.active_thread().map(str::to_owned) else {
            self.outbox.notice(
                chat,
                "No active session to delete. Use /init or send a text prompt.",
            );
            return Ok(());
        };
        let mut history = session.threads.clone();
        history.pop();
        if history.is_empty() {
            self.store.remove(&chat).await.map_err(store_error)?;
        } else {
            self.store
                .save(
                    chat.clone(),
                    SessionState {
                        session_ids: history,
                    },
                )
                .await
                .map_err(store_error)?;
        }
        self.sessions
            .delete_current(&chat)
            .expect("turn checked before write");
        tracing::info!(chat_id = ?chat, session_id = %id, "Deleted session");
        let next = self
            .sessions
            .chats
            .get(&chat)
            .and_then(|s| s.active_thread())
            .map_or_else(
                || "No sessions left. Use /init or send a text prompt.".to_owned(),
                |id| format!("Active session: {id}"),
            );
        self.outbox
            .notice(chat, format!("Deleted session: {id}\n{next}"));
        Ok(())
    }

    fn list_sessions(&mut self, chat: B::ChatId) {
        let text = self.sessions.chats.get(&chat).map_or_else(
            || "No sessions yet. Use /init or send a text prompt.".to_owned(),
            |session| {
                session
                    .threads
                    .iter()
                    .map(|id| {
                        format!(
                            "{id}{}",
                            if session.active_thread() == Some(id.as_str()) {
                                " (active)"
                            } else {
                                ""
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            },
        );
        self.outbox.notice(chat, text);
    }

    async fn start_turn(
        &mut self,
        chat: B::ChatId,
        owner: Option<B::UserId>,
        prompt: &str,
    ) -> BridgeResult<()> {
        if prompt.trim().is_empty() {
            return Err(user_error("prompt must not be blank"));
        }
        if self
            .sessions
            .chats
            .get(&chat)
            .is_some_and(|s| s.turn.is_some())
        {
            return Err(user_error(
                "a turn is already running; wait or use /interrupt",
            ));
        }
        let session_id = match self
            .sessions
            .chats
            .get(&chat)
            .and_then(|s| s.active_thread())
        {
            Some(id) => id.to_owned(),
            None => self.init(chat.clone(), false).await?,
        };
        let turn_id = self.agent.start_turn(&session_id, prompt).await?;
        tracing::info!(chat_id = ?chat, session_id = %session_id, turn_id = %turn_id, "Started turn");
        self.sessions.chats.entry(chat).or_default().turn = Some(ActiveTurn { id: turn_id, owner });
        Ok(())
    }

    async fn interrupt(&mut self, chat: B::ChatId) -> BridgeResult<()> {
        let mut interrupted = false;
        if let Some(handle) = self.running_execs.remove(&chat) {
            handle.abort();
            tracing::info!(chat_id = ?chat, "Cancelled one-off task");
            interrupted = true;
        }
        if let Some((session_id, turn_id)) = self.sessions.chats.get(&chat).and_then(|session| {
            Some((
                session.active_thread()?.to_owned(),
                session.turn.as_ref()?.id.clone(),
            ))
        }) {
            let tickets: Vec<_> = self
                .sessions
                .approvals
                .iter()
                .filter_map(|(ticket, pending)| (pending.chat_id == chat).then_some(*ticket))
                .collect();
            for ticket in tickets {
                if let Some(pending) = self.sessions.approvals.remove(&ticket) {
                    if let Some(message) = pending.message_id.clone() {
                        self.outbox.clear_approval(chat.clone(), message);
                    }
                    let request_id = pending.request_id.clone();
                    drop(pending);
                    self.agent
                        .respond_approval(&request_id, ApprovalDecision::Deny)
                        .await?;
                }
            }
            self.agent.interrupt(&session_id, &turn_id).await?;
            tracing::info!(chat_id = ?chat, session_id = %session_id, turn_id = %turn_id, "Requested turn interruption");
            interrupted = true;
        }
        self.outbox.notice(
            chat,
            if interrupted {
                "Interrupt requested."
            } else {
                "No operation is running."
            },
        );
        Ok(())
    }

    fn exec(&mut self, chat: B::ChatId, prompt: String) -> BridgeResult<()> {
        if prompt.trim().is_empty() {
            return Err(user_error("usage: /exec <nonblank prompt>"));
        }
        if self.running_execs.contains_key(&chat) {
            return Err(user_error(
                "a one-off task is already running; wait or use /interrupt",
            ));
        }
        let future = self.agent.exec(prompt);
        let job_chat = chat.clone();
        let handle = self
            .exec_jobs
            .spawn(async move { (job_chat, future.await) });
        self.running_execs.insert(chat.clone(), handle);
        tracing::info!(chat_id = ?chat, "Started one-off task");
        self.outbox.notice(chat, "Started one-off task.");
        Ok(())
    }

    fn exec_completed(&mut self, chat: B::ChatId, result: BridgeResult<ExecutionOutput>) {
        match result {
            Err(error) => self
                .outbox
                .notice(chat, format!("One-off task failed: {error}")),
            Ok(output) => {
                tracing::info!(chat_id = ?chat, status = %output.status, "One-off task completed");
                let empty = output.output.is_empty();
                if !empty {
                    self.outbox.output(chat.clone(), output.output);
                }
                if !output.success {
                    let detail = if output.stderr.trim().is_empty() {
                        format!("Agent exited with {}.", output.status)
                    } else {
                        format!("Agent exited with {}.\n{}", output.status, output.stderr)
                    };
                    self.outbox.notice(chat, detail);
                } else if empty {
                    self.outbox
                        .notice(chat, "One-off task completed without output.");
                }
            }
        }
    }

    async fn agent_event(&mut self, event: AgentEvent) -> BridgeResult<()> {
        match event {
            AgentEvent::Output {
                session_id,
                turn_id,
                output,
            } => {
                if let Some(chat) = self.sessions.route(&session_id, turn_id.as_deref()) {
                    self.outbox.output(chat, output);
                }
            }
            AgentEvent::TurnCompleted {
                session_id,
                turn_id,
                status,
                error,
            } => {
                if let Some(chat) = self.sessions.route(&session_id, Some(&turn_id)) {
                    tracing::info!(chat_id = ?chat, session_id = %session_id, turn_id = %turn_id, ?status, "Turn completed");
                    let had_error = error.is_some();
                    if let Some(error) = error {
                        self.outbox
                            .output(chat.clone(), vec![crate::AgentOutput::Error(error)]);
                    }
                    let messages: Vec<_> = self
                        .sessions
                        .approvals
                        .values()
                        .filter(|pending| {
                            pending.chat_id == chat
                                && pending.thread_id == session_id
                                && pending.turn_id == turn_id
                        })
                        .filter_map(|pending| pending.message_id.clone())
                        .collect();
                    self.sessions.finish_turn(&session_id, &turn_id);
                    for message in messages {
                        self.outbox.clear_approval(chat.clone(), message);
                    }
                    if status == TurnStatus::Interrupted {
                        self.outbox.notice(chat, "Turn interrupted.");
                    } else if status == TurnStatus::Failed && !had_error {
                        self.outbox.notice(chat, "Agent turn failed.");
                    }
                }
            }
            AgentEvent::ApprovalRequested {
                request_id,
                session_id,
                turn_id,
                description,
            } => {
                self.approval_requested(request_id, session_id, turn_id, description)
                    .await?;
            }
            AgentEvent::RequestResolved {
                session_id,
                request_id,
            } => {
                for (chat, message) in self.sessions.resolve_request(&session_id, &request_id) {
                    self.outbox.clear_approval(chat, message);
                }
            }
            AgentEvent::UnsupportedRequest {
                request_id,
                session_id,
                description,
            } => {
                self.agent.reject_request(&request_id).await?;
                if let Some(chat) = session_id
                    .as_deref()
                    .and_then(|id| self.sessions.chat_for_thread(id))
                {
                    self.outbox.notice(chat, format!("Agent requested unsupported interaction: {description}. The request was rejected."));
                }
            }
        }
        Ok(())
    }

    async fn approval_requested(
        &mut self,
        request_id: String,
        session_id: Option<String>,
        turn_id: Option<String>,
        description: String,
    ) -> BridgeResult<()> {
        let Some((session_id, turn_id)) = session_id.zip(turn_id) else {
            return self
                .agent
                .respond_approval(&request_id, ApprovalDecision::Deny)
                .await;
        };
        let Some(chat) = self.sessions.route(&session_id, Some(&turn_id)) else {
            return self
                .agent
                .respond_approval(&request_id, ApprovalDecision::Deny)
                .await;
        };
        let owner = self
            .sessions
            .chats
            .get(&chat)
            .and_then(|session| session.turn.as_ref())
            .and_then(|turn| turn.owner.clone());
        let Some(owner) = owner else {
            self.agent
                .respond_approval(&request_id, ApprovalDecision::Deny)
                .await?;
            self.outbox.notice(chat, "Approval declined: the request could not be associated with its initiating user and turn.");
            return Ok(());
        };
        let (ticket, live) =
            self.sessions
                .queue_approval(chat.clone(), request_id, session_id, turn_id, owner);
        tracing::debug!(chat_id = ?chat, ticket, "Queued approval request");
        self.outbox.approval(chat, ticket, description, live);
        Ok(())
    }

    async fn approval_delivered(
        &mut self,
        delivery: ApprovalDelivery<B::ChatId, B::MessageId>,
    ) -> BridgeResult<()> {
        match delivery.result {
            Ok(message) => {
                if !self
                    .sessions
                    .register_delivery(delivery.ticket, message.clone())
                {
                    self.sessions.approvals.remove(&delivery.ticket);
                    self.outbox.clear_approval(delivery.chat_id, message);
                }
            }
            Err(error) => {
                tracing::warn!(%error, "approval prompt delivery failed");
                if let Some(pending) = self.sessions.approvals.remove(&delivery.ticket) {
                    let current = self.sessions.pending_is_current(&pending);
                    let request_id = pending.request_id.clone();
                    drop(pending);
                    if current {
                        self.agent
                            .respond_approval(&request_id, ApprovalDecision::Deny)
                            .await?;
                        self.outbox.notice(
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
        chat: B::ChatId,
        message: B::MessageId,
        sender: Option<B::UserId>,
        decision: ApprovalDecision,
    ) -> BridgeResult<()> {
        let ticket = sender
            .as_ref()
            .and_then(|user| self.sessions.approval_ticket(&chat, &message, user));
        let Some(ticket) = ticket else {
            self.outbox.notice(
                chat,
                "This approval has expired or belongs to another user.",
            );
            return Ok(());
        };
        let pending = self
            .sessions
            .approvals
            .remove(&ticket)
            .expect("ticket still present");
        let request_id = pending.request_id.clone();
        drop(pending);
        self.outbox.clear_approval(chat, message);
        tracing::info!(ticket, ?decision, "Answered approval request");
        self.agent.respond_approval(&request_id, decision).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentOutput, BridgeError};
    use std::{
        collections::VecDeque,
        future::{Future, pending},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tokio::{
        sync::{Notify, mpsc},
        time::timeout,
    };

    #[derive(Default)]
    struct State {
        sent: Mutex<Vec<(u64, String)>>,
        calls: Mutex<Vec<String>>,
        approvals: Mutex<Vec<(String, ApprovalDecision)>>,
        next_message: AtomicUsize,
        shutdown: AtomicBool,
        held: AtomicBool,
        hold_rpc: AtomicBool,
        approval_gate: Notify,
        hold_chat: AtomicBool,
        fatal: AtomicBool,
        fail_send: AtomicBool,
        bot_started: AtomicBool,
        bot_dropped: AtomicBool,
        shutdown_started: AtomicBool,
        hold_shutdown: AtomicBool,
        shutdown_gate: Notify,
        agent_events: Mutex<VecDeque<AgentEvent>>,
        event_notify: Notify,
        event_count: AtomicUsize,
    }

    struct BotRunGuard(Arc<State>);

    impl Drop for BotRunGuard {
        fn drop(&mut self) {
            self.0.bot_dropped.store(true, Ordering::SeqCst);
        }
    }

    #[derive(Clone, Default)]
    struct FakeBot(Arc<State>);

    impl Bot for FakeBot {
        type ChatId = u64;
        type UserId = u64;
        type MessageId = usize;

        async fn run(&self, _events: mpsc::Sender<BotEvent<Self>>) -> BridgeResult<()> {
            self.0.bot_started.store(true, Ordering::SeqCst);
            let _guard = BotRunGuard(self.0.clone());
            pending().await
        }
        async fn send(&self, chat: &u64, output: &[AgentOutput]) -> BridgeResult<()> {
            if self.0.fail_send.load(Ordering::SeqCst) {
                return Err(io::Error::other("send failed").into());
            }
            if *chat == 1 && self.0.hold_chat.load(Ordering::SeqCst) {
                self.0.held.store(true, Ordering::SeqCst);
                self.0.approval_gate.notified().await;
            }
            self.0
                .sent
                .lock()
                .unwrap()
                .push((*chat, format!("{output:?}")));
            Ok(())
        }
        async fn send_notice(&self, chat: &u64, text: &str) -> BridgeResult<()> {
            self.0.sent.lock().unwrap().push((*chat, text.to_owned()));
            Ok(())
        }
        async fn request_approval(&self, chat: &u64, text: &str) -> BridgeResult<usize> {
            if self.0.hold_rpc.load(Ordering::SeqCst) {
                self.0.held.store(true, Ordering::SeqCst);
                self.0.approval_gate.notified().await;
            }
            self.0.sent.lock().unwrap().push((*chat, text.to_owned()));
            Ok(self.0.next_message.fetch_add(1, Ordering::SeqCst))
        }
        async fn clear_approval(&self, chat: &u64, message: &usize) -> BridgeResult<()> {
            self.0
                .sent
                .lock()
                .unwrap()
                .push((*chat, format!("clear:{message}")));
            Ok(())
        }
    }

    struct FakeAgent(Arc<State>);

    impl CodeAgent for FakeAgent {
        async fn next_event(&mut self) -> BridgeResult<Option<AgentEvent>> {
            loop {
                let notified = self.0.event_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(event) = self.0.agent_events.lock().unwrap().pop_front() {
                    self.0.event_count.fetch_add(1, Ordering::SeqCst);
                    return Ok(Some(event));
                }
                notified.await;
            }
        }
        async fn start_session(&mut self) -> BridgeResult<String> {
            let mut calls = self.0.calls.lock().unwrap();
            let id = format!(
                "session-{}",
                calls
                    .iter()
                    .filter(|call| call.starts_with("start_session"))
                    .count()
            );
            calls.push(format!("start_session:{id}"));
            Ok(id)
        }
        async fn start_turn(&mut self, session: &str, prompt: &str) -> BridgeResult<String> {
            self.0
                .calls
                .lock()
                .unwrap()
                .push(format!("start_turn:{session}:{prompt}"));
            if prompt == "broken" {
                return Err(io::Error::other("connection lost").into());
            }
            if self.0.hold_rpc.load(Ordering::SeqCst) {
                self.0.held.store(true, Ordering::SeqCst);
                pending().await
            } else {
                Ok(format!("turn-{session}"))
            }
        }
        async fn interrupt(&mut self, session: &str, turn: &str) -> BridgeResult<()> {
            self.0
                .calls
                .lock()
                .unwrap()
                .push(format!("interrupt:{session}:{turn}"));
            Ok(())
        }
        async fn respond_approval(
            &mut self,
            request: &str,
            decision: ApprovalDecision,
        ) -> BridgeResult<()> {
            self.0
                .approvals
                .lock()
                .unwrap()
                .push((request.to_owned(), decision));
            Ok(())
        }
        async fn reject_request(&mut self, request: &str) -> BridgeResult<()> {
            self.0
                .calls
                .lock()
                .unwrap()
                .push(format!("reject:{request}"));
            Ok(())
        }
        fn exec(
            &self,
            prompt: String,
        ) -> impl Future<Output = BridgeResult<ExecutionOutput>> + Send + 'static {
            self.0.calls.lock().unwrap().push(format!("exec:{prompt}"));
            let state = self.0.clone();
            async move {
                if prompt == "hold" {
                    state.held.store(true, Ordering::SeqCst);
                    pending().await
                }
                if prompt == "fail" {
                    return Err(io::Error::other("exec failed").into());
                }
                Ok(ExecutionOutput {
                    output: vec![AgentOutput::Message(prompt)],
                    success: true,
                    status: "ok".into(),
                    stderr: String::new(),
                })
            }
        }
        async fn shutdown(&mut self) -> BridgeResult<()> {
            self.0.shutdown_started.store(true, Ordering::SeqCst);
            if self.0.hold_shutdown.load(Ordering::SeqCst) {
                self.0.shutdown_gate.notified().await;
            }
            self.0.shutdown.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn is_connection_fatal(&self, _error: &BridgeError) -> bool {
            self.0.fatal.load(Ordering::SeqCst)
        }
    }

    fn runtime() -> (Runtime<FakeBot, FakeAgent>, Arc<State>) {
        let state = Arc::new(State::default());
        let (approval_sender, approval_deliveries) = mpsc::unbounded_channel();
        (
            Runtime {
                agent: FakeAgent(state.clone()),
                store: Memory::default(),
                allowed_user_id: None,
                sessions: Sessions::default(),
                outbox: Outbox::new(FakeBot(state.clone()), approval_sender),
                approval_deliveries,
                exec_jobs: JoinSet::new(),
                running_execs: HashMap::new(),
            },
            state,
        )
    }

    fn message(
        chat: u64,
        sender: Option<u64>,
        command: Option<Command>,
        text: Option<&str>,
    ) -> BotEvent<FakeBot> {
        IncomingMessage::Message {
            chat_id: chat,
            sender,
            command,
            text: text.map(str::to_owned),
        }
    }

    #[derive(Clone)]
    struct InputBot(
        FakeBot,
        Arc<Mutex<Option<mpsc::Receiver<BotEvent<InputBot>>>>>,
    );

    impl Bot for InputBot {
        type ChatId = u64;
        type UserId = u64;
        type MessageId = usize;

        async fn run(&self, sender: mpsc::Sender<BotEvent<Self>>) -> BridgeResult<()> {
            self.0.0.bot_started.store(true, Ordering::SeqCst);
            let mut input = self.1.lock().unwrap().take().unwrap();
            while let Some(event) = input.recv().await {
                sender.send(event).await.unwrap();
            }
            Ok(())
        }
        async fn send(&self, chat: &u64, output: &[AgentOutput]) -> BridgeResult<()> {
            self.0.send(chat, output).await
        }
        async fn send_notice(&self, chat: &u64, text: &str) -> BridgeResult<()> {
            self.0.send_notice(chat, text).await
        }
        async fn request_approval(&self, chat: &u64, text: &str) -> BridgeResult<usize> {
            self.0.request_approval(chat, text).await
        }
        async fn clear_approval(&self, chat: &u64, message: &usize) -> BridgeResult<()> {
            self.0.clear_approval(chat, message).await
        }
    }

    fn queue_event(state: &State, event: AgentEvent) {
        state.agent_events.lock().unwrap().push_back(event);
        state.event_notify.notify_one();
    }

    #[derive(Clone, Default)]
    struct TestStore(Arc<TestStoreState>);

    #[derive(Default)]
    struct TestStoreState {
        records: Mutex<HashMap<u64, SessionState>>,
        calls: Mutex<Vec<String>>,
        fail_list: AtomicBool,
        fail_save: AtomicBool,
        fail_remove: AtomicBool,
        hold_list: AtomicBool,
        list_started: AtomicBool,
        list_gate: Notify,
    }

    impl Store for TestStore {
        type ChatId = u64;
        type Session<'a> = SessionState;

        async fn load(&self, chat: &u64) -> BridgeResult<Option<SessionState>> {
            Ok(self.0.records.lock().unwrap().get(chat).cloned())
        }

        async fn list(&self) -> BridgeResult<Vec<(u64, SessionState)>> {
            self.0.list_started.store(true, Ordering::SeqCst);
            if self.0.hold_list.load(Ordering::SeqCst) {
                self.0.list_gate.notified().await;
            }
            if self.0.fail_list.load(Ordering::SeqCst) {
                return Err(io::Error::other("list failed").into());
            }
            self.0.calls.lock().unwrap().push("list".into());
            Ok(self
                .0
                .records
                .lock()
                .unwrap()
                .iter()
                .map(|(chat, state)| (*chat, state.clone()))
                .collect())
        }

        async fn save(&self, chat: u64, state: SessionState) -> BridgeResult<()> {
            self.0
                .calls
                .lock()
                .unwrap()
                .push(format!("save:{chat}:{:?}", state.session_ids));
            if self.0.fail_save.load(Ordering::SeqCst) {
                return Err(io::Error::other("save failed").into());
            }
            self.0.records.lock().unwrap().insert(chat, state);
            Ok(())
        }

        async fn remove(&self, chat: &u64) -> BridgeResult<()> {
            self.0.calls.lock().unwrap().push(format!("remove:{chat}"));
            if self.0.fail_remove.load(Ordering::SeqCst) {
                return Err(io::Error::other("remove failed").into());
            }
            self.0.records.lock().unwrap().remove(chat);
            Ok(())
        }
    }

    fn stored_runtime(store: TestStore) -> (Runtime<FakeBot, FakeAgent, TestStore>, Arc<State>) {
        let state = Arc::new(State::default());
        let (approval_sender, approval_deliveries) = mpsc::unbounded_channel();
        (
            Runtime {
                agent: FakeAgent(state.clone()),
                store,
                allowed_user_id: None,
                sessions: Sessions::default(),
                outbox: Outbox::new(FakeBot(state.clone()), approval_sender),
                approval_deliveries,
                exec_jobs: JoinSet::new(),
                running_execs: HashMap::new(),
            },
            state,
        )
    }

    struct BorrowedStore {
        history: SessionState,
    }

    impl Store for BorrowedStore {
        type ChatId = u64;
        type Session<'a> = &'a SessionState;

        async fn load<'a>(&'a self, chat: &u64) -> BridgeResult<Option<Self::Session<'a>>> {
            Ok((*chat == 1).then_some(&self.history))
        }

        async fn list(&self) -> BridgeResult<Vec<(u64, Self::Session<'_>)>> {
            Ok(vec![(1, &self.history)])
        }

        async fn save(&self, _chat: u64, _state: SessionState) -> BridgeResult<()> {
            Err(io::Error::other("read only").into())
        }

        async fn remove(&self, _chat: &u64) -> BridgeResult<()> {
            Err(io::Error::other("read only").into())
        }
    }

    async fn delivered(runtime: &mut Runtime<FakeBot, FakeAgent>) {
        let delivery = timeout(Duration::from_secs(1), runtime.approval_deliveries.recv())
            .await
            .unwrap()
            .unwrap();
        runtime.approval_delivered(delivery).await.unwrap();
    }

    #[tokio::test]
    async fn allowlist_precedes_every_side_effect() {
        let (mut runtime, state) = runtime();
        runtime.allowed_user_id = Some(7);
        for command in [
            Command::Init,
            Command::New,
            Command::Delete,
            Command::Sessions,
            Command::Interrupt,
            Command::Exec("task".into()),
        ] {
            runtime
                .incoming(message(1, Some(8), Some(command), None))
                .await
                .unwrap();
        }
        runtime
            .incoming(message(1, None, None, Some("prompt")))
            .await
            .unwrap();
        runtime
            .incoming(IncomingMessage::Approval {
                chat_id: 1,
                message_id: 0,
                sender: Some(8),
                decision: ApprovalDecision::Allow,
            })
            .await
            .unwrap();
        assert!(runtime.sessions.chats.is_empty());
        assert!(state.calls.lock().unwrap().is_empty());
        assert!(state.sent.lock().unwrap().is_empty());
        runtime.outbox.shutdown().await;
    }

    #[tokio::test]
    async fn routes_only_active_session_and_turn() {
        let (mut runtime, state) = runtime();
        runtime
            .incoming(message(1, Some(7), None, Some("hello")))
            .await
            .unwrap();
        runtime
            .incoming(message(2, Some(8), None, Some("world")))
            .await
            .unwrap();
        for (session, turn, text) in [
            ("session-0", Some("turn-session-0"), "yes"),
            ("session-0", Some("other"), "wrong turn"),
            ("session-1", None, "other chat"),
            ("missing", None, "missing"),
        ] {
            runtime
                .agent_event(AgentEvent::Output {
                    session_id: session.into(),
                    turn_id: turn.map(str::to_owned),
                    output: vec![AgentOutput::Message(text.into())],
                })
                .await
                .unwrap();
        }
        runtime.outbox.shutdown().await;
        let sent = state.sent.lock().unwrap();
        assert!(
            sent.iter()
                .any(|(chat, text)| *chat == 1 && text.contains("yes"))
        );
        assert!(
            sent.iter()
                .any(|(chat, text)| *chat == 2 && text.contains("other chat"))
        );
        assert!(
            !sent
                .iter()
                .any(|(_, text)| text.contains("wrong turn") || text.contains("missing"))
        );
    }

    #[tokio::test]
    async fn approval_requires_exact_chat_message_owner_session_and_turn() {
        let (mut runtime, state) = runtime();
        runtime
            .incoming(message(1, Some(7), None, Some("hello")))
            .await
            .unwrap();
        runtime
            .agent_event(AgentEvent::ApprovalRequested {
                request_id: "r1".into(),
                session_id: Some("session-0".into()),
                turn_id: Some("turn-session-0".into()),
                description: "approve?".into(),
            })
            .await
            .unwrap();
        delivered(&mut runtime).await;
        for (chat, msg, user) in [
            (2, 0, Some(7)),
            (1, 1, Some(7)),
            (1, 0, Some(8)),
            (1, 0, None),
        ] {
            runtime
                .incoming(IncomingMessage::Approval {
                    chat_id: chat,
                    message_id: msg,
                    sender: user,
                    decision: ApprovalDecision::Allow,
                })
                .await
                .unwrap();
        }
        assert!(state.approvals.lock().unwrap().is_empty());
        runtime
            .incoming(IncomingMessage::Approval {
                chat_id: 1,
                message_id: 0,
                sender: Some(7),
                decision: ApprovalDecision::Allow,
            })
            .await
            .unwrap();
        assert_eq!(
            *state.approvals.lock().unwrap(),
            [("r1".into(), ApprovalDecision::Allow)]
        );
        runtime.outbox.shutdown().await;
    }

    #[tokio::test]
    async fn unroutable_and_anonymous_requests_decline_and_resolution_expires_ticket() {
        let (mut runtime, state) = runtime();
        runtime
            .agent_event(AgentEvent::ApprovalRequested {
                request_id: "unknown".into(),
                session_id: None,
                turn_id: None,
                description: String::new(),
            })
            .await
            .unwrap();
        runtime
            .incoming(message(1, None, None, Some("hello")))
            .await
            .unwrap();
        runtime
            .agent_event(AgentEvent::ApprovalRequested {
                request_id: "anonymous".into(),
                session_id: Some("session-0".into()),
                turn_id: Some("turn-session-0".into()),
                description: String::new(),
            })
            .await
            .unwrap();
        assert_eq!(state.approvals.lock().unwrap().len(), 2);
        runtime
            .agent_event(AgentEvent::TurnCompleted {
                session_id: "session-0".into(),
                turn_id: "turn-session-0".into(),
                status: TurnStatus::Completed,
                error: None,
            })
            .await
            .unwrap();
        runtime
            .incoming(message(1, Some(7), None, Some("again")))
            .await
            .unwrap();
        runtime
            .agent_event(AgentEvent::ApprovalRequested {
                request_id: "r".into(),
                session_id: Some("session-0".into()),
                turn_id: Some("turn-session-0".into()),
                description: "approve?".into(),
            })
            .await
            .unwrap();
        delivered(&mut runtime).await;
        runtime
            .agent_event(AgentEvent::RequestResolved {
                session_id: "session-0".into(),
                request_id: "r".into(),
            })
            .await
            .unwrap();
        runtime
            .incoming(IncomingMessage::Approval {
                chat_id: 1,
                message_id: 0,
                sender: Some(7),
                decision: ApprovalDecision::Allow,
            })
            .await
            .unwrap();
        assert_eq!(state.approvals.lock().unwrap().len(), 2);
        runtime.outbox.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_cancels_in_flight_agent_rpc_and_calls_agent_shutdown() {
        let state = Arc::new(State::default());
        state.hold_rpc.store(true, Ordering::SeqCst);
        let bot = FakeBot(state.clone());
        let agent = FakeAgent(state.clone());
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (events, rx) = mpsc::channel(8);
        // A bot with an input channel keeps the bridge's dispatch loop active.
        #[derive(Clone)]
        struct InputBot(
            FakeBot,
            Arc<Mutex<Option<mpsc::Receiver<BotEvent<InputBot>>>>>,
        );
        impl Bot for InputBot {
            type ChatId = u64;
            type UserId = u64;
            type MessageId = usize;
            async fn run(&self, sender: mpsc::Sender<BotEvent<Self>>) -> BridgeResult<()> {
                let mut input = self.1.lock().unwrap().take().unwrap();
                while let Some(event) = input.recv().await {
                    sender.send(event).await.unwrap();
                }
                Ok(())
            }
            async fn send(&self, c: &u64, o: &[AgentOutput]) -> BridgeResult<()> {
                self.0.send(c, o).await
            }
            async fn send_notice(&self, c: &u64, t: &str) -> BridgeResult<()> {
                self.0.send_notice(c, t).await
            }
            async fn request_approval(&self, c: &u64, t: &str) -> BridgeResult<usize> {
                self.0.request_approval(c, t).await
            }
            async fn clear_approval(&self, c: &u64, m: &usize) -> BridgeResult<()> {
                self.0.clear_approval(c, m).await
            }
        }
        let input = InputBot(bot, Arc::new(Mutex::new(Some(rx))));
        let task = tokio::spawn(async move {
            Bridge::new(input, agent)
                .run(async { shutdown_rx.await.map_err(io::Error::other) })
                .await
        });
        events
            .send(IncomingMessage::Message {
                chat_id: 1,
                sender: Some(7),
                command: None,
                text: Some("hello".into()),
            })
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while !state.held.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        shutdown_tx.send(()).unwrap();
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(state.shutdown.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn slow_chat_does_not_block_other_chats_or_event_handling() {
        let (mut runtime, state) = runtime();
        state.hold_chat.store(true, Ordering::SeqCst);
        runtime
            .incoming(message(1, Some(7), None, Some("first")))
            .await
            .unwrap();
        runtime
            .incoming(message(2, Some(8), None, Some("second")))
            .await
            .unwrap();
        runtime
            .agent_event(AgentEvent::Output {
                session_id: "session-0".into(),
                turn_id: None,
                output: vec![AgentOutput::Message("held".into())],
            })
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while !state.held.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        runtime
            .agent_event(AgentEvent::Output {
                session_id: "session-1".into(),
                turn_id: None,
                output: vec![AgentOutput::Message("free".into())],
            })
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while !state
                .sent
                .lock()
                .unwrap()
                .iter()
                .any(|(chat, text)| *chat == 2 && text.contains("free"))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        state.approval_gate.notify_waiters();
        runtime.outbox.shutdown().await;
    }

    #[tokio::test]
    async fn exec_failure_is_reported_and_next_exec_can_start() {
        let (mut runtime, state) = runtime();
        runtime
            .incoming(message(
                1,
                Some(7),
                Some(Command::Exec("fail".into())),
                None,
            ))
            .await
            .unwrap();
        let completion = runtime
            .exec_jobs
            .join_next_with_id()
            .await
            .unwrap()
            .unwrap();
        let (id, (chat, result)) = completion;
        assert_eq!(runtime.running_execs[&chat].id(), id);
        runtime.running_execs.remove(&chat);
        runtime.exec_completed(chat, result);
        runtime
            .incoming(message(
                1,
                Some(7),
                Some(Command::Exec("success".into())),
                None,
            ))
            .await
            .unwrap();
        assert!(runtime.running_execs.contains_key(&1));
        runtime.exec_jobs.shutdown().await;
        runtime.outbox.shutdown().await;
        assert!(
            state
                .sent
                .lock()
                .unwrap()
                .iter()
                .any(|(_, text)| text.contains("One-off task failed: exec failed"))
        );
    }

    #[tokio::test]
    async fn agent_io_error_uses_connection_fatal_classification() {
        let (mut runtime, state) = runtime();
        state.fatal.store(true, Ordering::SeqCst);
        let error = runtime
            .incoming(message(1, Some(7), None, Some("broken")))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "connection lost");
        runtime
            .incoming(message(1, Some(7), Some(Command::Exec(" ".into())), None))
            .await
            .unwrap();
        runtime.outbox.shutdown().await;
        assert!(
            state
                .sent
                .lock()
                .unwrap()
                .iter()
                .any(|(_, text)| text.contains("usage: /exec"))
        );

        {
            let (mut runtime, state) = self::runtime();
            runtime
                .incoming(message(1, Some(7), None, Some("broken")))
                .await
                .unwrap();
            runtime.outbox.shutdown().await;
            assert!(
                state
                    .sent
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(_, text)| text.contains("Request failed: connection lost"))
            );
        }
    }

    #[tokio::test]
    async fn exec_interruption_cancels_only_that_chat() {
        let (mut runtime, state) = runtime();
        runtime
            .incoming(message(
                1,
                Some(7),
                Some(Command::Exec("hold".into())),
                None,
            ))
            .await
            .unwrap();
        runtime
            .incoming(message(
                2,
                Some(8),
                Some(Command::Exec("other".into())),
                None,
            ))
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while !state.held.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        runtime
            .incoming(message(1, Some(7), Some(Command::Interrupt), None))
            .await
            .unwrap();
        assert!(!runtime.running_execs.contains_key(&1));
        let completion = timeout(Duration::from_secs(1), async {
            loop {
                match runtime.exec_jobs.join_next_with_id().await.unwrap() {
                    Ok((id, (chat, result))) if chat == 2 => break (id, chat, result),
                    _ => continue,
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(runtime.running_execs[&completion.1].id(), completion.0);
        runtime.running_execs.remove(&completion.1);
        runtime.exec_completed(completion.1, completion.2);
        runtime
            .incoming(message(
                1,
                Some(7),
                Some(Command::Exec("again".into())),
                None,
            ))
            .await
            .unwrap();
        assert!(runtime.running_execs.contains_key(&1));
        runtime.exec_jobs.shutdown().await;
        runtime.outbox.shutdown().await;
        assert!(
            state
                .sent
                .lock()
                .unwrap()
                .iter()
                .any(|(chat, text)| *chat == 2 && text.contains("other"))
        );
    }

    #[tokio::test]
    async fn bot_success_and_error_are_propagated_once() {
        #[derive(Clone)]
        struct ClosingBot(FakeBot, bool);
        impl Bot for ClosingBot {
            type ChatId = u64;
            type UserId = u64;
            type MessageId = usize;
            async fn run(&self, _sender: mpsc::Sender<BotEvent<Self>>) -> BridgeResult<()> {
                if self.1 {
                    Err(io::Error::other("polling failed").into())
                } else {
                    Ok(())
                }
            }
            async fn send(&self, c: &u64, o: &[AgentOutput]) -> BridgeResult<()> {
                self.0.send(c, o).await
            }
            async fn send_notice(&self, c: &u64, t: &str) -> BridgeResult<()> {
                self.0.send_notice(c, t).await
            }
            async fn request_approval(&self, c: &u64, t: &str) -> BridgeResult<usize> {
                self.0.request_approval(c, t).await
            }
            async fn clear_approval(&self, c: &u64, m: &usize) -> BridgeResult<()> {
                self.0.clear_approval(c, m).await
            }
        }
        for fail in [false, true] {
            let state = Arc::new(State::default());
            let result = timeout(
                Duration::from_secs(1),
                Bridge::new(
                    ClosingBot(FakeBot(state.clone()), fail),
                    FakeAgent(state.clone()),
                )
                .run(pending()),
            )
            .await
            .unwrap();
            assert_eq!(result.is_err(), fail);
            if fail {
                assert_eq!(result.unwrap_err().to_string(), "polling failed");
            }
            assert!(state.shutdown.load(Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn outbox_preserves_chat_fifo_order() {
        let (mut runtime, state) = runtime();
        runtime.outbox.notice(1, "first");
        runtime
            .outbox
            .output(1, vec![AgentOutput::Message("second".into())]);
        runtime.outbox.notice(1, "third");
        runtime.outbox.shutdown().await;
        let sent = state.sent.lock().unwrap();
        assert_eq!(sent.len(), 3);
        assert!(sent[0].1.contains("first"));
        assert!(sent[1].1.contains("second"));
        assert!(sent[2].1.contains("third"));
    }

    #[tokio::test]
    async fn outbox_shutdown_cancels_stalled_delivery_after_one_second() {
        let (mut runtime, state) = runtime();
        state.hold_chat.store(true, Ordering::SeqCst);
        runtime
            .outbox
            .output(1, vec![AgentOutput::Message("stalled".into())]);
        timeout(Duration::from_secs(1), async {
            while !state.held.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        timeout(Duration::from_secs(2), runtime.outbox.shutdown())
            .await
            .unwrap();
        assert!(
            !state
                .sent
                .lock()
                .unwrap()
                .iter()
                .any(|(_, text)| text.contains("stalled"))
        );
    }

    #[tokio::test]
    async fn ready_shutdown_never_starts_bot_and_still_shuts_down_agent() {
        let state = Arc::new(State::default());
        Bridge::new(FakeBot(state.clone()), FakeAgent(state.clone()))
            .run(async { Ok(()) })
            .await
            .unwrap();
        assert!(!state.bot_started.load(Ordering::SeqCst));
        assert!(state.shutdown.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn dropping_bridge_run_cancels_bot_polling() {
        let state = Arc::new(State::default());
        let task = tokio::spawn(
            Bridge::new(FakeBot(state.clone()), FakeAgent(state.clone())).run(pending()),
        );
        timeout(Duration::from_secs(1), async {
            while !state.bot_started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        timeout(Duration::from_secs(1), async {
            while !state.bot_dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn shutdown_invalidates_queued_approval_before_agent_shutdown_finishes() {
        let state = Arc::new(State::default());
        state.hold_chat.store(true, Ordering::SeqCst);
        state.hold_shutdown.store(true, Ordering::SeqCst);
        let (events, receiver) = mpsc::channel(8);
        let bot = InputBot(FakeBot(state.clone()), Arc::new(Mutex::new(Some(receiver))));
        let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(
            Bridge::new(bot, FakeAgent(state.clone()))
                .run(async { shutdown_receiver.await.map_err(io::Error::other) }),
        );

        events
            .send(IncomingMessage::Message {
                chat_id: 1,
                sender: Some(7),
                command: None,
                text: Some("work".into()),
            })
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while !state
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("start_turn"))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        queue_event(
            &state,
            AgentEvent::Output {
                session_id: "session-0".into(),
                turn_id: None,
                output: vec![AgentOutput::Message("held output".into())],
            },
        );
        timeout(Duration::from_secs(1), async {
            while !state.held.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        queue_event(
            &state,
            AgentEvent::ApprovalRequested {
                request_id: "request".into(),
                session_id: Some("session-0".into()),
                turn_id: Some("turn-session-0".into()),
                description: "stale approval".into(),
            },
        );
        timeout(Duration::from_secs(1), async {
            while state.event_count.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::task::yield_now().await;
        shutdown_sender.send(()).unwrap();
        timeout(Duration::from_secs(1), async {
            while !state.shutdown_started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        state.approval_gate.notify_waiters();
        tokio::task::yield_now().await;
        assert!(
            !state
                .sent
                .lock()
                .unwrap()
                .iter()
                .any(|(_, text)| text.contains("stale approval"))
        );
        state.shutdown_gate.notify_waiters();
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(state.shutdown.load(Ordering::SeqCst));
        assert!(
            !state
                .sent
                .lock()
                .unwrap()
                .iter()
                .any(|(_, text)| text.contains("stale approval"))
        );
    }

    #[tokio::test]
    async fn output_delivery_failure_sends_visible_notice() {
        let (mut runtime, state) = runtime();
        state.fail_send.store(true, Ordering::SeqCst);
        runtime
            .outbox
            .output(1, vec![AgentOutput::Message("output".into())]);
        runtime.outbox.shutdown().await;
        assert!(
            state
                .sent
                .lock()
                .unwrap()
                .iter()
                .any(|(chat, text)| *chat == 1
                    && text.contains("Could not deliver agent output: send failed"))
        );
    }

    #[tokio::test]
    async fn new_delete_and_list_while_turn_runs_preserve_session() {
        let (mut runtime, state) = runtime();
        runtime
            .incoming(message(1, Some(7), None, Some("work")))
            .await
            .unwrap();
        runtime
            .incoming(message(1, Some(7), Some(Command::New), None))
            .await
            .unwrap();
        runtime
            .incoming(message(1, Some(7), Some(Command::Delete), None))
            .await
            .unwrap();
        runtime
            .incoming(message(1, Some(7), Some(Command::Sessions), None))
            .await
            .unwrap();
        assert_eq!(runtime.sessions.chats[&1].threads, ["session-0"]);
        assert!(runtime.sessions.chats[&1].turn.is_some());
        runtime.outbox.shutdown().await;
        let sent = state.sent.lock().unwrap();
        assert!(sent.iter().any(|(_, text)| text.contains("before /new")));
        assert!(sent.iter().any(|(_, text)| text.contains("before /delete")));
        assert!(
            sent.iter()
                .any(|(_, text)| text.contains("session-0 (active)"))
        );
    }

    #[tokio::test]
    async fn store_writes_precede_cache_changes_and_failures_are_fatal() {
        let store = TestStore::default();
        let (mut runtime, state) = stored_runtime(store.clone());
        store.0.fail_save.store(true, Ordering::SeqCst);
        let error = runtime
            .incoming(message(1, Some(7), Some(Command::Init), None))
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<StoreError>().is_some());
        assert_eq!(error.source().unwrap().to_string(), "save failed");
        assert!(runtime.sessions.chats.is_empty());
        assert!(store.0.records.lock().unwrap().is_empty());
        assert!(state.sent.lock().unwrap().is_empty());

        store.0.fail_save.store(false, Ordering::SeqCst);
        runtime
            .incoming(message(1, Some(7), Some(Command::Init), None))
            .await
            .unwrap();
        runtime
            .incoming(message(1, Some(7), Some(Command::New), None))
            .await
            .unwrap();
        assert_eq!(
            runtime.sessions.chats[&1].threads,
            ["session-1", "session-2"]
        );
        assert_eq!(
            store.0.records.lock().unwrap()[&1].session_ids,
            ["session-1", "session-2"]
        );

        store.0.fail_save.store(true, Ordering::SeqCst);
        assert!(
            runtime
                .incoming(message(1, Some(7), Some(Command::Delete), None))
                .await
                .is_err()
        );
        assert_eq!(runtime.sessions.chats[&1].threads.len(), 2);
        store.0.fail_save.store(false, Ordering::SeqCst);
        runtime
            .incoming(message(1, Some(7), Some(Command::Delete), None))
            .await
            .unwrap();
        assert_eq!(runtime.sessions.chats[&1].threads, ["session-1"]);
        assert_eq!(
            store.0.records.lock().unwrap()[&1].session_ids,
            ["session-1"]
        );

        store.0.fail_remove.store(true, Ordering::SeqCst);
        assert!(
            runtime
                .incoming(message(1, Some(7), Some(Command::Delete), None))
                .await
                .is_err()
        );
        assert_eq!(runtime.sessions.chats[&1].threads, ["session-1"]);
        store.0.fail_remove.store(false, Ordering::SeqCst);
        runtime
            .incoming(message(1, Some(7), Some(Command::Delete), None))
            .await
            .unwrap();
        assert!(!runtime.sessions.chats.contains_key(&1));
        assert!(store.0.records.lock().unwrap().is_empty());
        runtime.outbox.shutdown().await;
    }

    #[tokio::test]
    async fn implicit_session_is_persisted_before_turn_starts() {
        let store = TestStore::default();
        let (mut runtime, state) = stored_runtime(store.clone());
        store.0.fail_save.store(true, Ordering::SeqCst);
        assert!(
            runtime
                .incoming(message(1, Some(7), None, Some("prompt")))
                .await
                .is_err()
        );
        assert!(
            !state
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("start_turn"))
        );
        store.0.fail_save.store(false, Ordering::SeqCst);
        runtime
            .incoming(message(1, Some(7), None, Some("prompt")))
            .await
            .unwrap();
        assert_eq!(
            store.0.records.lock().unwrap()[&1].session_ids,
            ["session-1"]
        );
        assert!(
            state
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call == "start_turn:session-1:prompt")
        );
        runtime.outbox.shutdown().await;
    }

    #[tokio::test]
    async fn bridge_restores_stored_history_before_polling() {
        let store = TestStore::default();
        store.0.records.lock().unwrap().insert(
            1,
            SessionState {
                session_ids: vec!["old".into(), "current".into()],
            },
        );
        let state = Arc::new(State::default());
        let (events, receiver) = mpsc::channel(8);
        let bot = InputBot(FakeBot(state.clone()), Arc::new(Mutex::new(Some(receiver))));
        let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(
            Bridge::new(bot, FakeAgent(state.clone()))
                .with_store(store.clone())
                .run(async { shutdown_receiver.await.map_err(io::Error::other) }),
        );
        events
            .send(IncomingMessage::Message {
                chat_id: 1,
                sender: Some(7),
                command: Some(Command::Sessions),
                text: None,
            })
            .await
            .unwrap();
        events
            .send(IncomingMessage::Approval {
                chat_id: 1,
                message_id: 0,
                sender: Some(7),
                decision: ApprovalDecision::Allow,
            })
            .await
            .unwrap();
        events
            .send(IncomingMessage::Message {
                chat_id: 1,
                sender: Some(7),
                command: None,
                text: Some("continue".into()),
            })
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while !state
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call == "start_turn:current:continue")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        shutdown_sender.send(()).unwrap();
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            state
                .sent
                .lock()
                .unwrap()
                .iter()
                .any(|(_, text)| text.contains("old\ncurrent (active)"))
        );
        assert!(
            state
                .sent
                .lock()
                .unwrap()
                .iter()
                .any(|(_, text)| text.contains("approval has expired"))
        );
        assert!(state.approvals.lock().unwrap().is_empty());
        assert_eq!(
            store.0.records.lock().unwrap()[&1].session_ids,
            ["old", "current"]
        );
    }

    #[tokio::test]
    async fn bridge_accepts_borrowed_gat_session_snapshot() {
        let state = Arc::new(State::default());
        let store = BorrowedStore {
            history: SessionState {
                session_ids: vec!["borrowed".into()],
            },
        };
        let (events, receiver) = mpsc::channel(8);
        let bot = InputBot(FakeBot(state.clone()), Arc::new(Mutex::new(Some(receiver))));
        let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(
            Bridge::new(bot, FakeAgent(state.clone()))
                .with_store(store)
                .run(async { shutdown_receiver.await.map_err(io::Error::other) }),
        );
        events
            .send(IncomingMessage::Message {
                chat_id: 1,
                sender: Some(7),
                command: Some(Command::Sessions),
                text: None,
            })
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while !state
                .sent
                .lock()
                .unwrap()
                .iter()
                .any(|(_, text)| text.contains("borrowed (active)"))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        shutdown_sender.send(()).unwrap();
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn startup_list_failure_and_cancellation_shutdown_agent_without_polling() {
        let store = TestStore::default();
        store.0.fail_list.store(true, Ordering::SeqCst);
        let state = Arc::new(State::default());
        let error = Bridge::new(FakeBot(state.clone()), FakeAgent(state.clone()))
            .with_store(store.clone())
            .run(pending())
            .await
            .unwrap_err();
        assert_eq!(error.source().unwrap().to_string(), "list failed");
        assert!(state.shutdown.load(Ordering::SeqCst));
        assert!(!state.bot_started.load(Ordering::SeqCst));

        let store = TestStore::default();
        store.0.hold_list.store(true, Ordering::SeqCst);
        let state = Arc::new(State::default());
        let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(
            Bridge::new(FakeBot(state.clone()), FakeAgent(state.clone()))
                .with_store(store.clone())
                .run(async { shutdown_receiver.await.map_err(io::Error::other) }),
        );
        timeout(Duration::from_secs(1), async {
            while !store.0.list_started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        shutdown_sender.send(()).unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!state.bot_started.load(Ordering::SeqCst));
        assert!(state.shutdown.load(Ordering::SeqCst));
    }
}
