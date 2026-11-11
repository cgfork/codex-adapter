mod access;
mod bridge;
mod outbox;
mod sessions;

use std::{
    io::{self, Write},
    path::PathBuf,
    process::{ExitCode, ExitStatus},
};

use clap::{Parser, Subcommand};
use codexbridge::{BridgeResult, Codex};

#[derive(Debug, Parser)]
#[command(
    name = "adapter",
    version,
    about = "Run Codex tasks or bridge Codex to Telegram"
)]
struct Cli {
    #[arg(
        short = 'C',
        long,
        global = true,
        default_value = ".",
        value_name = "PATH"
    )]
    cwd: PathBuf,
    #[arg(long, global = true, default_value = "codex", value_name = "PATH")]
    codex: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Run a one-off Codex task and forward its raw output")]
    Exec {
        #[arg(value_name = "PROMPT", value_parser = nonblank_prompt)]
        prompt: String,
    },
    #[command(
        about = "Bridge Telegram chats to Codex (requires TELOXIDE_TOKEN)",
        after_help = "Proxy examples:\n  adapter bridge --proxy http://127.0.0.1:8080\n  adapter bridge --proxy socks5h://127.0.0.1:1080\n\nsocks5h resolves Telegram hostnames through the proxy.\nProxy selection: --proxy > TELOXIDE_PROXY > standard HTTPS_PROXY/ALL_PROXY via reqwest.\nTELOXIDE_PROXY affects only Telegram; standard proxy variables are inherited by Codex."
    )]
    Bridge {
        #[arg(
            long,
            value_name = "URL",
            help = "HTTP or SOCKS proxy for Telegram (overrides TELOXIDE_PROXY)"
        )]
        proxy: Option<String>,
        #[arg(
            long,
            value_name = "USER_ID",
            value_parser = clap::value_parser!(u64).range(1..),
            help = "Accept input only from this numeric Telegram account ID (not a username or chat ID)"
        )]
        allowed_user_id: Option<u64>,
    },
}

fn nonblank_prompt(prompt: &str) -> Result<String, String> {
    if prompt.trim().is_empty() {
        Err("prompt must not be blank".to_owned())
    } else {
        Ok(prompt.to_owned())
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("warn,adapter=info,tgbot=info,codexbridge=info")
            }),
        )
        .with_writer(io::stderr)
        .with_ansi(false)
        .init();
    match run(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            tracing::error!(%error, "Adapter failed");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> BridgeResult<u8> {
    let cwd = cli.cwd.canonicalize().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("invalid working directory {}: {error}", cli.cwd.display()),
        )
    })?;
    if !cwd.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("working directory is not a directory: {}", cwd.display()),
        )
        .into());
    }
    let codex = Codex::new(cwd).with_executable(cli.codex);
    let shutdown = async {
        tokio::signal::ctrl_c().await?;
        tracing::info!("Ctrl-C received; shutting down");
        Ok(())
    };
    match cli.command {
        Command::Exec { prompt } => {
            tracing::debug!("Starting one-off task");
            let output = codex
                .exec_with_shutdown(&prompt, shutdown)
                .await
                .map_err(|error| io::Error::other(format!("failed to execute Codex: {error}")))?;
            let Some(output) = output else {
                return Ok(130);
            };
            let code = exit_code(output.status);
            tracing::debug!(exit_code = code, "One-off task completed");
            let stdout_result = write_stream(&mut io::stdout().lock(), output.stdout.as_bytes());
            let stderr_result = write_stream(&mut io::stderr().lock(), output.stderr.as_bytes());
            stdout_result?;
            stderr_result?;
            Ok(code)
        }
        Command::Bridge {
            proxy,
            allowed_user_id,
        } => {
            bridge::run(
                codex,
                proxy.as_deref(),
                allowed_user_id.map(teloxide::types::UserId),
                shutdown,
            )
            .await?;
            Ok(0)
        }
    }
}

fn write_stream(stream: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    stream.write_all(bytes)?;
    stream.flush()
}

fn exit_code(status: ExitStatus) -> u8 {
    if let Some(code) = status.code() {
        return u8::try_from(code).unwrap_or(1);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return u8::try_from(128 + signal).unwrap_or(1);
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command, nonblank_prompt};

    #[test]
    fn globals_work_on_either_side_of_the_subcommand() {
        let cli =
            Cli::try_parse_from(["adapter", "-C", "/tmp", "exec", "hello", "--codex", "fake"])
                .unwrap();
        assert_eq!(cli.cwd.to_str(), Some("/tmp"));
        assert_eq!(cli.codex.to_str(), Some("fake"));
        assert!(matches!(cli.command, Command::Exec { prompt } if prompt == "hello"));
    }

    #[test]
    fn bridge_user_restriction_is_optional_and_accepts_positive_u64_ids() {
        let cli = Cli::try_parse_from(["adapter", "bridge"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Bridge {
                allowed_user_id: None,
                ..
            }
        ));
        for user_id in [1, 4_294_967_296, u64::MAX] {
            let cli = Cli::try_parse_from([
                "adapter",
                "bridge",
                "--allowed-user-id",
                &user_id.to_string(),
            ])
            .unwrap();
            assert!(matches!(
                cli.command,
                Command::Bridge {
                    allowed_user_id: Some(parsed),
                    ..
                } if parsed == user_id
            ));
        }
    }

    #[test]
    fn requires_a_subcommand_and_exactly_one_nonblank_prompt() {
        for arguments in [
            vec!["adapter"],
            vec!["adapter", "exec"],
            vec!["adapter", "exec", " \n"],
            vec!["adapter", "exec", "one", "two"],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
        assert_eq!(
            nonblank_prompt("  preserve this\n").unwrap(),
            "  preserve this\n"
        );
    }
}
