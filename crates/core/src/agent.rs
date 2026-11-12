use std::future::Future;

use crate::{ApprovalDecision, BridgeError, BridgeResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentOutput {
    Message(String),
    Plan(String),
    Command {
        command: String,
        output: String,
        exit_code: Option<i64>,
    },
    FilesChanged(Vec<String>),
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
}

/// Agent notifications independent of a provider's serialization format.
/// Session, turn, and request identifiers are opaque and must be preserved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEvent {
    Output {
        session_id: String,
        turn_id: Option<String>,
        output: Vec<AgentOutput>,
    },
    TurnCompleted {
        session_id: String,
        turn_id: String,
        status: TurnStatus,
        error: Option<String>,
    },
    ApprovalRequested {
        request_id: String,
        session_id: Option<String>,
        turn_id: Option<String>,
        description: String,
    },
    RequestResolved {
        session_id: String,
        request_id: String,
    },
    UnsupportedRequest {
        request_id: String,
        session_id: Option<String>,
        description: String,
    },
}

/// Result of an independent execution, including failures with empty output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionOutput {
    pub output: Vec<AgentOutput>,
    pub success: bool,
    pub status: String,
    pub stderr: String,
}

/// An initialized agent connection supporting sessions and independent tasks.
///
/// `next_event` must be cancellation safe: dropping the future must not discard
/// a pending event. Cancellation of an in-flight RPC ends the bridge, which then
/// calls `shutdown`. `exec` returns an owned future so independent tasks can run
/// alongside session traffic; dropping it must stop its work and reap children.
pub trait CodeAgent: Send + 'static {
    fn next_event(&mut self) -> impl Future<Output = BridgeResult<Option<AgentEvent>>> + Send;

    /// Create a session whose ID is unique within this agent backend.
    fn start_session(&mut self) -> impl Future<Output = BridgeResult<String>> + Send;

    /// Start a turn in a new or previously stored session. Implementations must
    /// load or resume the session when it is not yet open on this connection.
    fn start_turn(
        &mut self,
        session_id: &str,
        prompt: &str,
    ) -> impl Future<Output = BridgeResult<String>> + Send;

    fn interrupt(
        &mut self,
        session_id: &str,
        turn_id: &str,
    ) -> impl Future<Output = BridgeResult<()>> + Send;

    fn respond_approval(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> impl Future<Output = BridgeResult<()>> + Send;

    fn reject_request(&mut self, request_id: &str)
    -> impl Future<Output = BridgeResult<()>> + Send;

    fn exec(
        &self,
        prompt: String,
    ) -> impl Future<Output = BridgeResult<ExecutionOutput>> + Send + 'static;

    fn shutdown(&mut self) -> impl Future<Output = BridgeResult<()>> + Send;

    /// Whether an operation error leaves the connection unsafe to keep using.
    /// Implementations may identify recoverable request rejections here.
    fn is_connection_fatal(&self, _error: &BridgeError) -> bool {
        true
    }
}
