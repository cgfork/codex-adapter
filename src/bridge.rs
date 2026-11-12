use std::{future::Future, io};

use codebridge_core::{Bridge, BridgeResult};
use codexbridge::{AppServer, Codex, CodexAgent};
use teloxide::types::UserId;
use tgbot::TelegramBot;

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

    Bridge::new(TelegramBot::new(bot), CodexAgent::new(codex, server))
        .with_allowed_user_id(allowed_user_id)
        .run(shutdown)
        .await
}
