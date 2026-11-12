use std::{
    collections::VecDeque,
    error::Error,
    fmt,
    future::{Future, pending, ready},
    io,
    process::Stdio,
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout},
    time::timeout,
};

use crate::{BridgeResult, Codex};

#[derive(Debug)]
pub enum AppServerError {
    Rpc {
        method: String,
        error: Value,
    },
    Transport {
        source: Box<dyn Error + Send + Sync>,
    },
    InvalidResponse {
        method: String,
        message: &'static str,
    },
}

impl AppServerError {
    pub fn is_connection_fatal(&self) -> bool {
        !matches!(self, Self::Rpc { .. })
    }

    fn transport(source: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        Self::Transport {
            source: source.into(),
        }
    }
}

impl fmt::Display for AppServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc { method, error } => write!(formatter, "{method}: {error}"),
            Self::Transport { source } => {
                write!(formatter, "Codex app-server transport failed: {source}")
            }
            Self::InvalidResponse { method, message } => write!(formatter, "{method}: {message}"),
        }
    }
}

impl Error for AppServerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport { source } => Some(source.as_ref()),
            _ => None,
        }
    }
}

pub struct AppServer {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
    queued: VecDeque<Value>,
    next_id: u64,
}

impl AppServer {
    pub async fn start(codex: &Codex) -> BridgeResult<Self> {
        Ok(Self::start_with_shutdown(codex, pending())
            .await?
            .expect("pending shutdown cannot complete"))
    }

    pub async fn start_with_shutdown(
        codex: &Codex,
        shutdown: impl Future<Output = io::Result<()>>,
    ) -> BridgeResult<Option<Self>> {
        tokio::pin!(shutdown);
        tokio::select! {
            biased;
            result = &mut shutdown => {
                result.map_err(AppServerError::transport)?;
                tracing::info!("Codex app-server startup cancelled");
                return Ok(None);
            }
            () = ready(()) => {}
        }
        let mut child = codex
            .command()
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(AppServerError::transport)?;
        tracing::info!(pid = child.id(), "Codex app-server started");
        let stdin = child.stdin.take().ok_or_else(|| {
            AppServerError::transport(io::Error::other("app-server stdin is unavailable"))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            AppServerError::transport(io::Error::other("app-server stdout is unavailable"))
        })?;
        let mut server = Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout).lines(),
            queued: VecDeque::new(),
            next_id: 1,
        };

        let initialized = tokio::select! {
            biased;
            result = &mut shutdown => {
                server.shutdown().await?;
                result.map_err(AppServerError::transport)?;
                return Ok(None);
            }
            result = server.initialize() => result,
        };
        if let Err(error) = initialized {
            tracing::warn!("Codex app-server initialization failed; stopping process");
            server
                .child
                .kill()
                .await
                .map_err(AppServerError::transport)?;
            return Err(error);
        }
        tracing::info!("Codex app-server initialized");
        Ok(Some(server))
    }

    async fn initialize(&mut self) -> BridgeResult<()> {
        self.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "codebridge",
                    "title": "Codebridge Telegram Bridge",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )
        .await?;
        self.write(&json!({ "method": "initialized" })).await
    }

    pub async fn request(&mut self, method: &str, params: Value) -> BridgeResult<Value> {
        let id = self.next_id;
        self.next_id += 1;
        tracing::debug!(method, rpc_id = id, "Sending Codex RPC request");
        timeout(Duration::from_secs(30), async {
            self.write(&json!({ "id": id, "method": method, "params": params }))
                .await?;
            loop {
                let response = self.read().await?.ok_or_else(|| {
                    AppServerError::transport(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Codex app-server closed stdout while waiting for a response",
                    ))
                })?;
                if response.get("id") == Some(&json!(id)) && response.get("method").is_none() {
                    if let Some(error) = response.get("error") {
                        tracing::warn!(method, rpc_id = id, "Codex RPC request failed");
                        return Err(AppServerError::Rpc {
                            method: method.to_owned(),
                            error: error.clone(),
                        }
                        .into());
                    }
                    tracing::debug!(method, rpc_id = id, "Received Codex RPC response");
                    return response.get("result").cloned().ok_or_else(|| {
                        AppServerError::InvalidResponse {
                            method: method.to_owned(),
                            message: "response has no result",
                        }
                        .into()
                    });
                }
                self.queued.push_back(response);
            }
        })
        .await
        .map_err(|error| {
            tracing::warn!(method, rpc_id = id, "Codex RPC request timed out");
            AppServerError::transport(error)
        })?
    }

    pub async fn next_event(&mut self) -> BridgeResult<Option<Value>> {
        if let Some(event) = self.queued.pop_front() {
            return Ok(Some(event));
        }
        self.read().await
    }

    pub async fn respond(&mut self, id: Value, result: Value) -> BridgeResult<()> {
        tracing::debug!(rpc_id = ?id, "Responding to Codex RPC request");
        self.write(&json!({ "id": id, "result": result })).await
    }

    pub async fn reject_request(&mut self, id: Value) -> BridgeResult<()> {
        tracing::debug!(rpc_id = ?id, "Rejecting unsupported Codex RPC request");
        self.write(&json!({
            "id": id,
            "error": { "code": -32601, "message": "Request is not supported by codebridge" }
        }))
        .await
    }

    pub async fn start_thread(&mut self, codex: &Codex) -> BridgeResult<String> {
        let response = self
            .request(
                "thread/start",
                json!({
                    "cwd": codex.working_directory()
                }),
            )
            .await?;
        response["thread"]["id"]
            .as_str()
            .map(|thread_id| {
                tracing::debug!(thread_id, "Codex thread started");
                thread_id.to_owned()
            })
            .ok_or_else(|| {
                AppServerError::InvalidResponse {
                    method: "thread/start".to_owned(),
                    message: "response has no thread ID",
                }
                .into()
            })
    }

    pub async fn start_turn(&mut self, thread_id: &str, prompt: &str) -> BridgeResult<String> {
        let response = self
            .request(
                "turn/start",
                json!({
                    "threadId": thread_id,
                    "input": [{ "type": "text", "text": prompt }]
                }),
            )
            .await?;
        response["turn"]["id"]
            .as_str()
            .map(|turn_id| {
                tracing::debug!(thread_id, turn_id, "Codex turn started");
                turn_id.to_owned()
            })
            .ok_or_else(|| {
                AppServerError::InvalidResponse {
                    method: "turn/start".to_owned(),
                    message: "response has no turn ID",
                }
                .into()
            })
    }

    pub async fn interrupt(&mut self, thread_id: &str, turn_id: &str) -> BridgeResult<()> {
        tracing::debug!(thread_id, turn_id, "Interrupting Codex turn");
        self.request(
            "turn/interrupt",
            json!({ "threadId": thread_id, "turnId": turn_id }),
        )
        .await?;
        Ok(())
    }

    pub async fn shutdown(&mut self) -> BridgeResult<()> {
        tracing::info!(pid = self.child.id(), "Stopping Codex app-server");
        // Dropping the pipe sends EOF even if the child is no longer reading.
        self.stdin.take();
        match timeout(Duration::from_secs(1), self.child.wait()).await {
            Ok(status) => {
                let status = status.map_err(AppServerError::transport)?;
                tracing::info!(
                    success = status.success(),
                    exit_code = status.code(),
                    "Codex app-server stopped"
                );
            }
            Err(_) => {
                tracing::warn!(
                    pid = self.child.id(),
                    "Codex app-server shutdown timed out; killing process"
                );
                self.child.kill().await.map_err(AppServerError::transport)?;
            }
        }
        Ok(())
    }

    async fn read(&mut self) -> BridgeResult<Option<Value>> {
        while let Some(line) = self
            .stdout
            .next_line()
            .await
            .map_err(AppServerError::transport)?
        {
            if !line.trim().is_empty() {
                let message: Value =
                    serde_json::from_str(&line).map_err(AppServerError::transport)?;
                tracing::trace!(
                    method = message.get("method").and_then(|value| value.as_str()),
                    "Received Codex app-server message"
                );
                return Ok(Some(message));
            }
        }
        tracing::debug!("Codex app-server stdout closed");
        Ok(None)
    }

    async fn write(&mut self, message: &Value) -> BridgeResult<()> {
        let mut line = serde_json::to_vec(message).map_err(AppServerError::transport)?;
        line.push(b'\n');
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            AppServerError::transport(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "app-server stdin is closed",
            ))
        })?;
        stdin
            .write_all(&line)
            .await
            .map_err(AppServerError::transport)?;
        stdin.flush().await.map_err(AppServerError::transport)?;
        Ok(())
    }
}
