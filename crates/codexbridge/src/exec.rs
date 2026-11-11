use std::{
    future::{Future, pending, ready},
    io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

use crate::BridgeResult;

#[derive(Clone, Debug)]
pub struct Codex {
    executable: PathBuf,
    working_directory: PathBuf,
}

#[derive(Debug)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: ExitStatus,
}

impl Codex {
    pub fn new(working_directory: impl Into<PathBuf>) -> Self {
        Self {
            executable: PathBuf::from("codex"),
            working_directory: working_directory.into(),
        }
    }

    pub fn with_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = executable.into();
        self
    }

    pub(crate) fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    pub(crate) fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .current_dir(&self.working_directory)
            .kill_on_drop(true);
        command
    }

    pub async fn exec(&self, prompt: &str) -> BridgeResult<ExecOutput> {
        Ok(self
            .exec_with_shutdown(prompt, pending())
            .await?
            .expect("pending shutdown cannot complete"))
    }

    pub async fn exec_with_shutdown(
        &self,
        prompt: &str,
        shutdown: impl Future<Output = io::Result<()>>,
    ) -> BridgeResult<Option<ExecOutput>> {
        if prompt.trim().is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "exec prompt is empty").into());
        }

        tokio::pin!(shutdown);
        tokio::select! {
            biased;
            result = &mut shutdown => {
                result?;
                tracing::info!("Codex exec cancelled before startup");
                return Ok(None);
            }
            () = ready(()) => {}
        }

        let mut child = self
            .command()
            .args(["exec", "--json", "--color", "never", "--ephemeral", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        tracing::debug!(pid = child.id(), "Codex exec started");
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("Codex stdin is unavailable"))?;
        let write_prompt = async move {
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.shutdown().await
        };
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("Codex stdout is unavailable"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("Codex stderr is unavailable"))?;
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();
        let completed = tokio::select! {
            biased;
            result = &mut shutdown => {
                tracing::info!(pid = child.id(), "Stopping Codex exec for shutdown");
                child.kill().await?;
                result?;
                return Ok(None);
            }
            output = async {
                tokio::join!(write_prompt, stdout.read_to_end(&mut stdout_bytes), stderr.read_to_end(&mut stderr_bytes), child.wait())
            } => output,
        };
        let (written, stdout_read, stderr_read, status) = completed;
        let status = status?;
        stdout_read?;
        stderr_read?;
        if status.success() {
            written?;
        }
        tracing::debug!(
            success = status.success(),
            exit_code = status.code(),
            "Codex exec finished"
        );

        Ok(Some(ExecOutput {
            stdout: String::from_utf8(stdout_bytes)?,
            stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
            status,
        }))
    }
}
