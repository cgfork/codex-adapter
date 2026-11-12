use std::{error::Error, io, process::ExitCode};

use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(
                    "warn,codebridge=info,codebridge_core=info,tgbot=info,codexbridge=info",
                )
            }),
        )
        .with_writer(io::stderr)
        .with_ansi(false)
        .init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => {
            tracing::error!("Telegram receiver failed");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let token = std::env::var("TELOXIDE_TOKEN")?;
    let bot = tgbot::create_bot(token, None)?;
    let (sender, mut receiver) = mpsc::channel(100);

    let receive_events = async {
        while let Some(event) = receiver.recv().await {
            println!("{event:?}");
        }
    };

    let (result, ()) = tokio::join!(tgbot::run(bot, sender), receive_events);
    result?;
    Ok(())
}
