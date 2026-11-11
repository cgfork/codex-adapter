use teloxide::{
    types::{CallbackQuery, ChatId, Message},
    utils::command::BotCommands,
};

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
