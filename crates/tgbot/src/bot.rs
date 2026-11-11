use std::error::Error;

use teloxide::{
    Bot, RequestError,
    dispatching::{Dispatcher, UpdateFilterExt},
    dptree,
    payloads::SendMessageSetters,
    requests::Requester,
    types::{CallbackQuery, Message, ParseMode, Update},
    utils::command::BotCommands,
};
use tokio::sync::mpsc::Sender;

use crate::{Command, IncomingMessage, OutgoingMessage, render_jsonl};

type HandlerResult = Result<(), Box<dyn Error + Send + Sync>>;

pub async fn run(bot: Bot, events: Sender<IncomingMessage>) -> Result<(), RequestError> {
    tracing::info!("Initializing Telegram bot");
    let me = bot.get_me().await?;
    bot.set_my_commands(Command::bot_commands()).await?;
    tracing::info!("Telegram bot initialized");

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(receive_message))
        .branch(Update::filter_callback_query().endpoint(receive_callback_query));

    tracing::info!("Starting Telegram update polling");
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![events, me.username().to_owned()])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
    tracing::info!("Telegram update polling stopped");

    Ok(())
}

pub async fn send(
    bot: &Bot,
    message: &OutgoingMessage,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    tracing::debug!(chat_id = message.chat_id.0, "Sending Telegram output");
    for html in render_jsonl(&message.stdout)? {
        bot.send_message(message.chat_id, html)
            .parse_mode(ParseMode::Html)
            .await?;
    }

    Ok(())
}

async fn receive_message(
    message: Message,
    bot_username: String,
    events: Sender<IncomingMessage>,
) -> HandlerResult {
    tracing::debug!(
        chat_id = message.chat.id.0,
        message_id = message.id.0,
        "Received Telegram message"
    );
    events
        .send(IncomingMessage::from_message(message, &bot_username))
        .await?;
    Ok(())
}

async fn receive_callback_query(
    bot: Bot,
    query: CallbackQuery,
    events: Sender<IncomingMessage>,
) -> HandlerResult {
    tracing::debug!(
        user_id = query.from.id.0,
        "Received Telegram callback query"
    );
    let acknowledgement = bot.answer_callback_query(query.id.clone()).await;

    if let Some(message) = IncomingMessage::from_callback_query(query) {
        events.send(message).await?;
    }

    acknowledgement?;
    Ok(())
}
