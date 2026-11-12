use std::{
    collections::HashMap,
    hash::Hash,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::{sync::mpsc, task::JoinSet, time::timeout};

use crate::{AgentOutput, Bot, BridgeResult};

pub(crate) struct ApprovalDelivery<C, M> {
    pub ticket: u64,
    pub chat_id: C,
    pub result: BridgeResult<M>,
}

enum Delivery<M> {
    Output(Vec<AgentOutput>),
    Notice(String),
    Approval {
        ticket: u64,
        text: String,
        live: Arc<AtomicBool>,
    },
    ClearApproval(M),
}

pub(crate) struct Outbox<B: Bot> {
    bot: B,
    chats: HashMap<B::ChatId, mpsc::UnboundedSender<Delivery<B::MessageId>>>,
    jobs: JoinSet<()>,
    approvals: mpsc::UnboundedSender<ApprovalDelivery<B::ChatId, B::MessageId>>,
}

impl<B: Bot> Outbox<B>
where
    B::ChatId: Eq + Hash,
{
    pub fn new(
        bot: B,
        approvals: mpsc::UnboundedSender<ApprovalDelivery<B::ChatId, B::MessageId>>,
    ) -> Self {
        Self {
            bot,
            chats: HashMap::new(),
            jobs: JoinSet::new(),
            approvals,
        }
    }

    pub fn output(&mut self, chat: B::ChatId, output: Vec<AgentOutput>) {
        self.enqueue(chat, Delivery::Output(output));
    }
    pub fn notice(&mut self, chat: B::ChatId, text: impl Into<String>) {
        self.enqueue(chat, Delivery::Notice(text.into()));
    }
    pub fn approval(&mut self, chat: B::ChatId, ticket: u64, text: String, live: Arc<AtomicBool>) {
        self.enqueue(chat, Delivery::Approval { ticket, text, live });
    }
    pub fn clear_approval(&mut self, chat: B::ChatId, message: B::MessageId) {
        self.enqueue(chat, Delivery::ClearApproval(message));
    }

    fn enqueue(&mut self, chat: B::ChatId, delivery: Delivery<B::MessageId>) {
        let delivery = match self.chats.get(&chat) {
            Some(sender) => match sender.send(delivery) {
                Ok(()) => return,
                Err(error) => error.0,
            },
            None => delivery,
        };
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let bot = self.bot.clone();
        let approvals = self.approvals.clone();
        let worker_chat = chat.clone();
        self.jobs.spawn(async move {
            while let Some(delivery) = receiver.recv().await {
                match delivery {
                    Delivery::Output(output) => {
                        if let Err(error) = bot.send(&worker_chat, &output).await {
                            tracing::warn!(%error, "could not deliver agent output");
                            if let Err(notice_error) = bot.send_notice(&worker_chat, &format!("Could not deliver agent output: {error}")).await {
                                tracing::warn!(%notice_error, "could not deliver output error notice");
                            }
                        }
                    }
                    Delivery::Notice(text) => {
                        if let Err(error) = bot.send_notice(&worker_chat, &text).await {
                            tracing::warn!(%error, "could not deliver bridge notice");
                        }
                    }
                    Delivery::Approval { ticket, text, live } => {
                        if !live.load(Ordering::Acquire) { continue; }
                        let result = bot.request_approval(&worker_chat, &text).await;
                        if !live.load(Ordering::Acquire) {
                            if let Ok(message) = result { let _ = bot.clear_approval(&worker_chat, &message).await; }
                            continue;
                        }
                        let _ = approvals.send(ApprovalDelivery { ticket, chat_id: worker_chat.clone(), result });
                    }
                    Delivery::ClearApproval(message) => {
                        if let Err(error) = bot.clear_approval(&worker_chat, &message).await {
                            tracing::warn!(%error, "could not clear approval controls");
                        }
                    }
                }
            }
        });
        sender.send(delivery).expect("new outbox receiver is alive");
        self.chats.insert(chat, sender);
    }

    pub async fn shutdown(&mut self) {
        self.chats.clear();
        if timeout(Duration::from_secs(1), async {
            while let Some(result) = self.jobs.join_next().await {
                if let Err(error) = result {
                    tracing::warn!(%error, "outbox task failed");
                }
            }
        })
        .await
        .is_err()
        {
            self.jobs.shutdown().await;
        }
    }
}
