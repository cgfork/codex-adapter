use std::{fmt::Debug, future::Future, hash::Hash};

use tokio::sync::mpsc;

use crate::{AgentOutput, BridgeResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Sessions,
    Interrupt,
    Init,
    New,
    Delete,
    Exec(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

/// A normalized bot event. `sender` identifies a human acting as themselves;
/// providers must use `None` for anonymous, impersonated, or bot senders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IncomingMessage<C, U, M> {
    Message {
        chat_id: C,
        sender: Option<U>,
        command: Option<Command>,
        text: Option<String>,
    },
    Approval {
        chat_id: C,
        message_id: M,
        sender: Option<U>,
        decision: ApprovalDecision,
    },
}

pub type BotEvent<B> =
    IncomingMessage<<B as Bot>::ChatId, <B as Bot>::UserId, <B as Bot>::MessageId>;

/// A chat transport. Clones share access to the same bot account.
///
/// `run` must stop polling when its future is dropped. Sending methods may run
/// concurrently for different chats; the bridge preserves order within a chat.
/// Dropping any sending future must cancel that operation safely.
pub trait Bot: Clone + Send + Sync + 'static {
    type ChatId: Clone + Debug + Eq + Hash + Send + Sync + 'static;
    type UserId: Clone + Debug + Eq + Hash + Send + Sync + 'static;
    type MessageId: Clone + Debug + Eq + Hash + Send + Sync + 'static;

    fn run(
        &self,
        events: mpsc::Sender<BotEvent<Self>>,
    ) -> impl Future<Output = BridgeResult<()>> + Send;

    fn send(
        &self,
        chat_id: &Self::ChatId,
        output: &[AgentOutput],
    ) -> impl Future<Output = BridgeResult<()>> + Send;

    fn send_notice(
        &self,
        chat_id: &Self::ChatId,
        text: &str,
    ) -> impl Future<Output = BridgeResult<()>> + Send;

    fn request_approval(
        &self,
        chat_id: &Self::ChatId,
        text: &str,
    ) -> impl Future<Output = BridgeResult<Self::MessageId>> + Send;

    fn clear_approval(
        &self,
        chat_id: &Self::ChatId,
        message_id: &Self::MessageId,
    ) -> impl Future<Output = BridgeResult<()>> + Send;
}
