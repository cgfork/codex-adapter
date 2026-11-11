mod app_server;
mod exec;
mod output;

pub use app_server::{AppServer, AppServerError};
pub use exec::{Codex, ExecOutput};
pub use output::{AgentOutput, JsonlError, parse_jsonl};

pub type BridgeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
