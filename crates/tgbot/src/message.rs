use codebridge_core::{
    ApprovalDecision, BotEvent, Command as CoreCommand, IncomingMessage as CoreIncomingMessage,
};
use teloxide::{
    types::{CallbackQuery, ChatId, Message},
    utils::command::BotCommands,
};

use crate::TelegramBot;

#[derive(Clone, Debug, PartialEq, Eq, BotCommands)]
#[command(rename_rule = "lowercase")]
pub enum Command {
    #[command(description = "List sessions")]
    Sessions,
    #[command(description = "Interrupt the current operation")]
    Interrupt,
    #[command(description = "Initialize a session")]
    Init,
    #[command(description = "Start a new session")]
    New,
    #[command(description = "Delete the current session")]
    Delete,
    #[command(description = "Run a one-off task: /exec <prompt>")]
    Exec(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalSignal {
    Allow,
    Deny,
}

impl ApprovalSignal {
    pub fn from_callback_data(data: &str) -> Option<Self> {
        match data {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    pub const fn callback_data(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

#[derive(Clone, Debug)]
pub enum IncomingMessage {
    Raw(Box<Message>),
    Command {
        command: Command,
        message: Box<Message>,
    },
    ApprovalSignal {
        signal: ApprovalSignal,
        query: Box<CallbackQuery>,
    },
}

impl IncomingMessage {
    pub fn from_message(message: Message, bot_username: &str) -> Self {
        let command = message
            .text()
            .and_then(|text| Command::parse(text, bot_username).ok());

        match command {
            Some(command) => Self::Command {
                command,
                message: Box::new(message),
            },
            None => Self::Raw(Box::new(message)),
        }
    }

    pub fn from_callback_query(query: CallbackQuery) -> Option<Self> {
        let signal = ApprovalSignal::from_callback_data(query.data.as_deref()?)?;
        Some(Self::ApprovalSignal {
            signal,
            query: Box::new(query),
        })
    }

    pub fn into_core(self) -> Option<BotEvent<TelegramBot>> {
        match self {
            Self::Raw(message) => Some(CoreIncomingMessage::Message {
                chat_id: message.chat.id,
                sender: human_sender(&message),
                command: None,
                text: message.text().map(str::to_owned),
            }),
            Self::Command { command, message } => Some(CoreIncomingMessage::Message {
                chat_id: message.chat.id,
                sender: human_sender(&message),
                command: Some(command.into()),
                text: message.text().map(str::to_owned),
            }),
            Self::ApprovalSignal { signal, query } => {
                let message = query.regular_message()?;
                Some(CoreIncomingMessage::Approval {
                    chat_id: message.chat.id,
                    message_id: message.id,
                    sender: (!query.from.is_bot).then_some(query.from.id),
                    decision: match signal {
                        ApprovalSignal::Allow => ApprovalDecision::Allow,
                        ApprovalSignal::Deny => ApprovalDecision::Deny,
                    },
                })
            }
        }
    }
}

fn human_sender(message: &Message) -> Option<teloxide::types::UserId> {
    if message.sender_chat.is_some() {
        return None;
    }
    message
        .from
        .as_ref()
        .and_then(|user| (!user.is_bot).then_some(user.id))
}

impl From<Command> for CoreCommand {
    fn from(command: Command) -> Self {
        match command {
            Command::Sessions => Self::Sessions,
            Command::Interrupt => Self::Interrupt,
            Command::Init => Self::Init,
            Command::New => Self::New,
            Command::Delete => Self::Delete,
            Command::Exec(prompt) => Self::Exec(prompt),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingMessage {
    pub chat_id: ChatId,
    pub stdout: String,
}

impl OutgoingMessage {
    pub fn from_stdout(chat_id: ChatId, stdout: impl Into<String>) -> Self {
        Self {
            chat_id,
            stdout: stdout.into(),
        }
    }
}
