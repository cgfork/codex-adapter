//! Shared contracts and a bridge for connecting chat bots to coding agents.
//!
//! Implement [`Bot`] and [`CodeAgent`], then compose them with [`Bridge`]. Bot
//! identifiers are associated types, and agent identifiers are opaque strings.
//! This crate does not depend on a chat SDK or an agent wire protocol.
//! Session histories use [`Store`], defaulting to the shared in-memory [`Memory`].

mod agent;
mod bot;
mod bridge;
mod outbox;
pub mod session;
mod sessions;

pub use agent::{AgentEvent, AgentOutput, CodeAgent, ExecutionOutput, TurnStatus};
pub use bot::{ApprovalDecision, Bot, BotEvent, Command, IncomingMessage};
pub use bridge::Bridge;
pub use session::{Memory, Session, SessionState, Store};

pub type BridgeError = Box<dyn std::error::Error + Send + Sync>;
pub type BridgeResult<T> = Result<T, BridgeError>;
