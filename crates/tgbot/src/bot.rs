use std::{error::Error, fmt, time::Duration};

use codebridge_core::{AgentOutput, Bot as CoreBot, BotEvent, BridgeResult};
use teloxide::{
    Bot, RequestError,
    payloads::{GetUpdatesSetters, SendMessageSetters},
    requests::Requester,
    types::{
        AllowedUpdate, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ParseMode,
        UpdateKind, UserId,
    },
    utils::command::BotCommands,
};
use tokio::{sync::mpsc::Sender, time::sleep};

use crate::{
    ApprovalSignal, Command, IncomingMessage, OutgoingMessage, render_jsonl, render_output,
};

#[derive(Clone)]
pub struct TelegramBot {
    bot: Bot,
}

pub struct TelegramTransportError {
    operation: &'static str,
    source: RequestError,
}

impl TelegramTransportError {
    fn new(operation: &'static str, source: RequestError) -> Self {
        Self { operation, source }
    }
}

impl fmt::Display for TelegramTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.operation)
    }
}

impl fmt::Debug for TelegramTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TelegramTransportError")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}

impl Error for TelegramTransportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

impl TelegramBot {
    pub fn new(bot: Bot) -> Self {
        Self { bot }
    }
}

impl CoreBot for TelegramBot {
    type ChatId = ChatId;
    type UserId = UserId;
    type MessageId = MessageId;

    async fn run(&self, events: Sender<BotEvent<Self>>) -> BridgeResult<()> {
        poll_updates(self.bot.clone(), events, IncomingMessage::into_core)
            .await
            .map_err(|error| TelegramTransportError::new("Telegram polling failed", error))?;
        Ok(())
    }

    async fn send(&self, chat_id: &ChatId, output: &[AgentOutput]) -> BridgeResult<()> {
        send_html(&self.bot, *chat_id, render_output(output))
            .await
            .map_err(|error| {
                TelegramTransportError::new("Telegram output delivery failed", error).into()
            })
    }

    async fn send_notice(&self, chat_id: &ChatId, text: &str) -> BridgeResult<()> {
        for part in split_plain_text(text) {
            self.bot
                .send_message(*chat_id, part)
                .await
                .map_err(|error| {
                    TelegramTransportError::new("Telegram notice delivery failed", error)
                })?;
        }
        Ok(())
    }

    async fn request_approval(&self, chat_id: &ChatId, text: &str) -> BridgeResult<MessageId> {
        let keyboard = InlineKeyboardMarkup::new([[
            InlineKeyboardButton::callback("Allow", ApprovalSignal::Allow.callback_data()),
            InlineKeyboardButton::callback("Deny", ApprovalSignal::Deny.callback_data()),
        ]]);
        let description = truncate_text(text, 4096);
        Ok(self
            .bot
            .send_message(*chat_id, description)
            .reply_markup(keyboard)
            .await
            .map_err(|error| {
                TelegramTransportError::new("Telegram approval delivery failed", error)
            })?
            .id)
    }

    async fn clear_approval(&self, chat_id: &ChatId, message_id: &MessageId) -> BridgeResult<()> {
        self.bot
            .edit_message_reply_markup(*chat_id, *message_id)
            .await
            .map_err(|error| {
                TelegramTransportError::new("Telegram approval cleanup failed", error)
            })?;
        Ok(())
    }
}

pub async fn run(bot: Bot, events: Sender<IncomingMessage>) -> Result<(), RequestError> {
    tokio::select! {
        result = poll_updates(bot, events, Some) => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

async fn poll_updates<T, F>(bot: Bot, events: Sender<T>, normalize: F) -> Result<(), RequestError>
where
    T: Send,
    F: Fn(IncomingMessage) -> Option<T>,
{
    tracing::info!("Initializing Telegram bot");
    let me = bot.get_me().await?;
    bot.set_my_commands(Command::bot_commands()).await?;
    match bot.get_webhook_info().await {
        Ok(info) if info.url.is_some() => {
            if bot.delete_webhook().await.is_err() {
                tracing::warn!("Could not remove Telegram webhook before polling");
            }
        }
        Err(_) => tracing::warn!("Could not inspect Telegram webhook before polling"),
        _ => {}
    }
    let username = me.username().to_owned();
    tracing::info!("Starting Telegram update polling");

    let mut offset = 0;
    let mut consecutive_failures = 0_u32;
    loop {
        if events.is_closed() {
            break;
        }
        let updates = match bot
            .get_updates()
            .offset(offset)
            .timeout(10)
            .allowed_updates(vec![AllowedUpdate::Message, AllowedUpdate::CallbackQuery])
            .await
        {
            Ok(updates) => {
                consecutive_failures = 0;
                updates
            }
            Err(error) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                tracing::warn!(
                    consecutive_failures,
                    "Telegram polling request failed; retrying"
                );
                sleep(polling_retry_delay(&error, consecutive_failures)).await;
                continue;
            }
        };
        for update in updates {
            offset = offset.max(next_offset(update.id.0));
            let incoming = match update.kind {
                UpdateKind::Message(message) => {
                    Some(IncomingMessage::from_message(message, &username))
                }
                UpdateKind::CallbackQuery(query) => {
                    if bot.answer_callback_query(query.id.clone()).await.is_err() {
                        tracing::warn!("Could not acknowledge Telegram callback");
                    }
                    IncomingMessage::from_callback_query(query)
                }
                _ => None,
            };
            if let Some(event) = incoming.and_then(&normalize) {
                if events.send(event).await.is_err() {
                    tracing::info!("Telegram event receiver closed");
                    return Ok(());
                }
            }
        }
    }
    tracing::info!("Telegram update polling stopped");
    Ok(())
}

fn next_offset(id: u32) -> i32 {
    i32::try_from(id).unwrap_or(i32::MAX - 1).saturating_add(1)
}

fn polling_retry_delay(error: &RequestError, consecutive_failures: u32) -> Duration {
    match error {
        RequestError::RetryAfter(seconds) => seconds.duration(),
        _ => Duration::from_secs(2_u64.pow(consecutive_failures.min(4))),
    }
}

pub async fn send(
    bot: &Bot,
    message: &OutgoingMessage,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    send_html(bot, message.chat_id, render_jsonl(&message.stdout)?).await?;
    Ok(())
}

async fn send_html(bot: &Bot, chat_id: ChatId, parts: Vec<String>) -> Result<(), RequestError> {
    for html in parts {
        bot.send_message(chat_id, html)
            .parse_mode(ParseMode::Html)
            .await?;
    }
    Ok(())
}

fn split_plain_text(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut length = 0;
    for character in text.chars() {
        let width = character.len_utf16();
        if length + width > 4096 {
            parts.push(std::mem::take(&mut part));
            length = 0;
        }
        part.push(character);
        length += width;
    }
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

fn truncate_text(text: &str, limit: usize) -> String {
    let mut result = String::new();
    let mut length = 0;
    for character in text.chars() {
        if length + character.len_utf16() > limit {
            break;
        }
        result.push(character);
        length += character.len_utf16();
    }
    result
}

#[cfg(test)]
mod tests {
    use std::{error::Error as _, time::Duration};

    use teloxide::{RequestError, types::Seconds};

    use super::{
        TelegramTransportError, next_offset, polling_retry_delay, split_plain_text, truncate_text,
    };

    #[test]
    fn plain_notices_split_on_telegram_utf16_limit() {
        let text = "😀".repeat(4097);
        let parts = split_plain_text(&text);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts.concat(), text);
        assert!(parts.iter().all(|part| part.encode_utf16().count() <= 4096));
        assert_eq!(truncate_text(&text, 4096).encode_utf16().count(), 4096);
    }

    #[test]
    fn update_offset_advances_past_received_updates() {
        assert_eq!(next_offset(41), 42);
        assert_eq!(next_offset(i32::MAX as u32), i32::MAX);
    }

    #[test]
    fn transport_error_hides_sensitive_response_but_preserves_source() {
        let source = RequestError::InvalidJson {
            source: serde_json::from_str::<serde_json::Value>("{")
                .unwrap_err()
                .into(),
            raw: "bot-token-secret".into(),
        };
        let error = TelegramTransportError::new("Telegram polling failed", source);
        assert_eq!(error.to_string(), "Telegram polling failed");
        assert!(!format!("{error:?}").contains("bot-token-secret"));
        let source = error
            .source()
            .unwrap()
            .downcast_ref::<RequestError>()
            .unwrap();
        assert!(matches!(source, RequestError::InvalidJson { .. }));
    }

    #[test]
    fn polling_uses_telegram_retry_after_before_exponential_backoff() {
        let limit = RequestError::RetryAfter(Seconds::from_seconds(37));
        assert_eq!(polling_retry_delay(&limit, 1), Duration::from_secs(37));
        assert_eq!(polling_retry_delay(&limit, 10), Duration::from_secs(37));
        let other = RequestError::InvalidJson {
            source: serde_json::from_str::<serde_json::Value>("{")
                .unwrap_err()
                .into(),
            raw: "{}".into(),
        };
        assert_eq!(polling_retry_delay(&other, 1), Duration::from_secs(2));
        assert_eq!(polling_retry_delay(&other, 10), Duration::from_secs(16));
    }
}
