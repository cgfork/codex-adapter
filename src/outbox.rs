use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_json::json;
use teloxide::{
    Bot,
    payloads::SendMessageSetters,
    requests::Requester,
    types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MessageId},
};
use tgbot::{ApprovalSignal, OutgoingMessage};
use tokio::{sync::mpsc, task::JoinSet, time::timeout};

pub(crate) struct ApprovalDelivery {
    pub(crate) ticket: u64,
    pub(crate) chat_id: ChatId,
    pub(crate) result: Result<MessageId, String>,
}

#[derive(Debug)]
enum Delivery {
    Output(String),
    Approval {
        ticket: u64,
        text: String,
        live: Arc<AtomicBool>,
    },
    ClearApproval(MessageId),
}

pub(crate) struct Outbox {
    bot: Bot,
    chats: HashMap<ChatId, mpsc::UnboundedSender<Delivery>>,
    jobs: JoinSet<()>,
    approvals: mpsc::UnboundedSender<ApprovalDelivery>,
}

impl Outbox {
    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        self.chats.is_empty() && self.jobs.is_empty()
    }

    pub(crate) fn new(bot: Bot, approvals: mpsc::UnboundedSender<ApprovalDelivery>) -> Self {
        Self {
            bot,
            chats: HashMap::new(),
            jobs: JoinSet::new(),
            approvals,
        }
    }

    pub(crate) fn output(&mut self, chat_id: ChatId, stdout: String) {
        self.enqueue(chat_id, Delivery::Output(stdout));
    }

    pub(crate) fn notice(&mut self, chat_id: ChatId, text: &str) {
        self.output(
            chat_id,
            json!({"type":"item.completed","item":{
                "type":"command_execution","command":"adapter","aggregated_output":text
            }})
            .to_string(),
        );
    }

    pub(crate) fn approval(
        &mut self,
        chat_id: ChatId,
        ticket: u64,
        text: String,
        live: Arc<AtomicBool>,
    ) {
        self.enqueue(chat_id, Delivery::Approval { ticket, text, live });
    }

    pub(crate) fn clear_approval(&mut self, chat_id: ChatId, message_id: MessageId) {
        self.enqueue(chat_id, Delivery::ClearApproval(message_id));
    }

    fn enqueue(&mut self, chat_id: ChatId, delivery: Delivery) {
        let delivery = match self.chats.get(&chat_id) {
            Some(sender) => match sender.send(delivery) {
                Ok(()) => return,
                Err(error) => error.0,
            },
            None => delivery,
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        let bot = self.bot.clone();
        let approvals = self.approvals.clone();
        self.jobs.spawn(async move {
            drain_chat(receiver, |delivery| {
                deliver(&bot, chat_id, &approvals, delivery)
            })
            .await;
        });
        sender.send(delivery).expect("new outbox receiver is alive");
        self.chats.insert(chat_id, sender);
    }

    pub(crate) async fn shutdown(&mut self) {
        self.chats.clear();
        let drained = timeout(Duration::from_secs(1), async {
            while let Some(result) = self.jobs.join_next().await {
                if let Err(error) = result {
                    tracing::error!(%error, "Telegram outbox task failed");
                }
            }
        })
        .await;
        if drained.is_err() {
            tracing::warn!(
                pending_tasks = self.jobs.len(),
                "Telegram outbox drain timed out; cancelling deliveries"
            );
            self.jobs.shutdown().await;
        }
    }
}

async fn drain_chat<F, DeliveryFuture>(mut receiver: mpsc::UnboundedReceiver<Delivery>, mut send: F)
where
    F: FnMut(Delivery) -> DeliveryFuture,
    DeliveryFuture: Future<Output = ()>,
{
    while let Some(delivery) = receiver.recv().await {
        send(delivery).await;
    }
}

async fn deliver(
    bot: &Bot,
    chat_id: ChatId,
    approvals: &mpsc::UnboundedSender<ApprovalDelivery>,
    delivery: Delivery,
) {
    match delivery {
        Delivery::Output(stdout) => {
            let message = OutgoingMessage::from_stdout(chat_id, stdout);
            if let Err(error) = tgbot::send(bot, &message).await {
                tracing::warn!(chat_id = chat_id.0, %error, "Could not send Codex output");
                if let Err(error) = bot
                    .send_message(chat_id, format!("Could not deliver Codex output: {error}"))
                    .await
                {
                    tracing::warn!(chat_id = chat_id.0, %error, "Could not deliver Telegram error notice");
                }
            }
        }
        Delivery::Approval { ticket, text, live } => {
            if !live.load(Ordering::Acquire) {
                return;
            }
            let keyboard = InlineKeyboardMarkup::new([[
                InlineKeyboardButton::callback("Allow", ApprovalSignal::Allow.callback_data()),
                InlineKeyboardButton::callback("Deny", ApprovalSignal::Deny.callback_data()),
            ]]);
            let result = bot
                .send_message(chat_id, text)
                .reply_markup(keyboard)
                .await
                .map(|message| message.id)
                .map_err(|error| error.to_string());
            if !live.load(Ordering::Acquire) {
                if let Ok(message_id) = result {
                    if let Err(error) = bot.edit_message_reply_markup(chat_id, message_id).await {
                        tracing::warn!(chat_id = chat_id.0, message_id = message_id.0, %error, "Could not remove expired approval buttons");
                    }
                }
                return;
            }
            let _ = approvals.send(ApprovalDelivery {
                ticket,
                chat_id,
                result,
            });
        }
        Delivery::ClearApproval(message_id) => {
            if let Err(error) = bot.edit_message_reply_markup(chat_id, message_id).await {
                tracing::warn!(chat_id = chat_id.0, message_id = message_id.0, %error, "Could not remove approval buttons");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use teloxide::{
        Bot,
        types::{ChatId, MessageId},
    };
    use tokio::{
        sync::{Notify, mpsc, oneshot},
        time::timeout,
    };

    use super::{Delivery, Outbox, drain_chat};

    #[tokio::test]
    async fn shutdown_drains_queued_deliveries() {
        let (approvals, _) = mpsc::unbounded_channel();
        let mut outbox = Outbox::new(Bot::new("test-token"), approvals);
        let (sender, receiver) = mpsc::unbounded_channel();
        let (delivered, mut received) = mpsc::unbounded_channel();
        outbox.chats.insert(ChatId(1), sender.clone());
        outbox.jobs.spawn(async move {
            drain_chat(receiver, |delivery| {
                let delivered = delivered.clone();
                async move {
                    let Delivery::ClearApproval(message_id) = delivery else {
                        panic!("unexpected delivery");
                    };
                    delivered.send(message_id).unwrap();
                }
            })
            .await;
        });
        sender.send(Delivery::ClearApproval(MessageId(1))).unwrap();
        sender.send(Delivery::ClearApproval(MessageId(2))).unwrap();
        drop(sender);

        outbox.shutdown().await;
        assert_eq!(received.recv().await, Some(MessageId(1)));
        assert_eq!(received.recv().await, Some(MessageId(2)));
        assert_eq!(received.recv().await, None);
        assert!(outbox.chats.is_empty());
        assert!(outbox.jobs.is_empty());
    }

    #[tokio::test]
    async fn shutdown_waits_for_stalled_deliveries_to_be_cancelled() {
        let (approvals, _) = mpsc::unbounded_channel();
        let mut outbox = Outbox::new(Bot::new("test-token"), approvals);
        let (started, ready) = oneshot::channel();
        let (held, released) = oneshot::channel::<()>();
        outbox.jobs.spawn(async move {
            let _held = held;
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();

        timeout(Duration::from_secs(3), outbox.shutdown())
            .await
            .expect("shutdown must not wait indefinitely for delivery");
        assert!(outbox.jobs.is_empty());
        assert!(released.await.is_err());
    }

    #[tokio::test]
    async fn chat_deliveries_wait_for_the_preceding_delivery() {
        let (sender, receiver) = mpsc::unbounded_channel();
        let (started, mut observed) = mpsc::unbounded_channel();
        let release = Arc::new(Notify::new());
        let release_first = release.clone();
        let worker = tokio::spawn(async move {
            drain_chat(receiver, |delivery| {
                let started = started.clone();
                let release = release_first.clone();
                async move {
                    let Delivery::ClearApproval(message_id) = delivery else {
                        panic!("unexpected delivery")
                    };
                    started.send(message_id).unwrap();
                    if message_id == MessageId(1) {
                        release.notified().await;
                    }
                }
            })
            .await;
        });
        sender.send(Delivery::ClearApproval(MessageId(1))).unwrap();
        sender.send(Delivery::ClearApproval(MessageId(2))).unwrap();
        assert_eq!(observed.recv().await, Some(MessageId(1)));
        tokio::task::yield_now().await;
        assert!(observed.try_recv().is_err());
        release.notify_one();
        assert_eq!(observed.recv().await, Some(MessageId(2)));
        drop(sender);
        worker.await.unwrap();
    }
}
