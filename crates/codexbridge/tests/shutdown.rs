#![cfg(unix)]

use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use codexbridge::{AppServer, Codex};
use tokio::time::{Instant, sleep, timeout};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    codex: Codex,
}

impl Fixture {
    fn new(script: &str) -> Self {
        let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "codexbridge-shutdown-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let executable = root.join("codex");
        fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '%s' \"$$\" > pid\n{script}\n"),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            codex: Codex::new(&root).with_executable(executable),
            root,
        }
    }

    async fn spawned(&self) {
        timeout(Duration::from_secs(5), async {
            while !self.root.join("pid").exists() {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("child did not start");
    }

    fn assert_reaped(&self) {
        let pid = fs::read_to_string(self.root.join("pid")).unwrap();
        assert!(
            !Command::new("kill")
                .args(["-0", &pid])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success(),
            "child {pid} survived cleanup"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const INITIALIZE: &str = "IFS= read -r request || exit 1\nprintf '%s\\n' '{\"id\":1,\"result\":{}}'\nIFS= read -r initialized || exit 1\n";

#[tokio::test]
async fn ready_shutdown_is_polled_before_spawning() {
    let codex = Codex::new("/").with_executable("/nonexistent/codex");
    assert!(
        codex
            .exec_with_shutdown("prompt", async { Ok(()) })
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        AppServer::start_with_shutdown(&codex, async { Ok(()) })
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn exec_shutdown_and_signal_errors_reap_the_child() {
    for signal_error in [false, true] {
        let fixture = Fixture::new("printf 'partial output'\nexec sleep 30");
        let shutdown = async {
            fixture.spawned().await;
            if signal_error {
                Err(io::Error::other("signal failed"))
            } else {
                Ok(())
            }
        };
        let result = timeout(
            Duration::from_secs(5),
            fixture.codex.exec_with_shutdown("prompt", shutdown),
        )
        .await
        .unwrap();
        if signal_error {
            assert_eq!(result.unwrap_err().to_string(), "signal failed");
        } else {
            assert!(result.unwrap().is_none());
        }
        fixture.assert_reaped();
    }
}

#[tokio::test]
async fn startup_shutdown_and_signal_errors_reap_the_initializing_child() {
    for signal_error in [false, true] {
        let fixture = Fixture::new("IFS= read -r request || exit 1\nexec sleep 30");
        let shutdown = async {
            fixture.spawned().await;
            if signal_error {
                Err(io::Error::other("signal failed"))
            } else {
                Ok(())
            }
        };
        let result = timeout(
            Duration::from_secs(5),
            AppServer::start_with_shutdown(&fixture.codex, shutdown),
        )
        .await
        .unwrap();
        if signal_error {
            assert!(result.err().unwrap().to_string().contains("signal failed"));
        } else {
            assert!(result.unwrap().is_none());
        }
        fixture.assert_reaped();
    }
}

#[tokio::test]
async fn shutdown_allows_eof_exit_and_is_repeatable() {
    let fixture = Fixture::new(&format!(
        "{INITIALIZE}while IFS= read -r request; do :; done\nprintf done > graceful"
    ));
    let mut server = AppServer::start(&fixture.codex).await.unwrap();
    timeout(Duration::from_secs(5), server.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fs::read_to_string(fixture.root.join("graceful")).unwrap(),
        "done"
    );
    server.shutdown().await.unwrap();
    fixture.assert_reaped();
}

#[tokio::test]
async fn shutdown_kills_and_reaps_an_unresponsive_child_after_a_bounded_wait() {
    let fixture = Fixture::new(&format!("{INITIALIZE}exec sleep 30"));
    let mut server = AppServer::start(&fixture.codex).await.unwrap();
    let started = Instant::now();
    timeout(Duration::from_secs(5), server.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(900));
    server.shutdown().await.unwrap();
    fixture.assert_reaped();
}
