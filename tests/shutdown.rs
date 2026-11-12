#![cfg(unix)]

use std::{
    fs::{self, File},
    io::{self, Read},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEADLINE: Duration = Duration::from_secs(8);
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    codebridge: Option<Child>,
}

impl Fixture {
    fn new(script: &str) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "codebridge shutdown {} {timestamp} {}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let executable = root.join("fake codex");
        fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$CODEBRIDGE_TEST_ROOT/pid\"\n{script}\n"),
        )
        .unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            root,
            codebridge: None,
        }
    }

    fn start(&mut self, arguments: &[&str]) {
        let child = Command::new(env!("CARGO_BIN_EXE_codebridge"))
            .process_group(0)
            .env_clear()
            .env("PATH", "")
            .env("TELOXIDE_TOKEN", "123456:local-shutdown-test")
            .env("CODEBRIDGE_TEST_ROOT", &self.root)
            .arg("--cwd")
            .arg(&self.root)
            .arg("--codex")
            .arg(self.root.join("fake codex"))
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(File::create(self.root.join("stdout")).unwrap())
            .stderr(File::create(self.root.join("stderr")).unwrap())
            .spawn()
            .unwrap();
        self.codebridge = Some(child);
    }

    fn diagnostics(&self) -> String {
        format!(
            "stdout: {}\nstderr: {}",
            fs::read_to_string(self.root.join("stdout")).unwrap_or_default(),
            fs::read_to_string(self.root.join("stderr")).unwrap_or_default()
        )
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.root.join("ready").exists() {
                return;
            }
            self.assert_running();
            assert!(
                Instant::now() < deadline,
                "fake Codex never became ready; {}",
                self.diagnostics()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_running(&mut self) {
        let status = self.codebridge.as_mut().unwrap().try_wait().unwrap();
        assert!(
            status.is_none(),
            "codebridge exited before readiness: {status:?}; {}",
            self.diagnostics()
        );
    }

    fn interrupt(&mut self) -> ExitStatus {
        self.interrupt_target(false)
    }

    fn interrupt_target(&mut self, process_group: bool) -> ExitStatus {
        self.assert_running();
        let pid = self.codebridge.as_ref().unwrap().id();
        let sent = if process_group {
            Command::new("/bin/kill")
                .args(["-INT", "--", &format!("-{pid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        } else {
            signal(pid, "-INT")
        };
        assert!(sent, "failed to signal codebridge PID {pid}");
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(status) = self.codebridge.as_mut().unwrap().try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "codebridge did not stop after Ctrl-C; {}",
                self.diagnostics()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_codex_reaped(&self) {
        let pid = self.codex_pid().expect("fake Codex must record its PID");
        // kill -0 also sees a zombie, so this checks that the codebridge waited for
        // its child, rather than merely sending a termination signal.
        assert!(
            !signal(pid, "-0"),
            "Codex PID {pid} remains alive or unreaped; {}",
            self.diagnostics()
        );
    }

    fn codex_pid(&self) -> Option<u32> {
        fs::read_to_string(self.root.join("pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = self.codebridge.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Also clean up on a failing assertion against an implementation which
        // leaves its Codex child running after the codebridge itself exits.
        if let Some(pid) = self.codex_pid() {
            let _ = signal(pid, "-KILL");
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn signal(pid: u32, signal: &str) -> bool {
    Command::new("/bin/kill")
        .args([signal, &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn exec_ctrl_c_exits_130_and_reaps_codex() {
    let mut fixture =
        Fixture::new("printf 'ready\\n' > \"$CODEBRIDGE_TEST_ROOT/ready\"\nexec /bin/sleep 300");
    fixture.start(&["exec", "wait for interruption"]);
    fixture.wait_ready();
    let status = fixture.interrupt();
    assert_eq!(status.code(), Some(130), "{}", fixture.diagnostics());
    fixture.assert_codex_reaped();
}

#[test]
fn bridge_ctrl_c_during_initialize_closes_stdin_and_reaps_codex() {
    let mut fixture = Fixture::new(
        r#"
IFS= read -r initialize || exit 1
printf 'ready\n' > "$CODEBRIDGE_TEST_ROOT/ready"
while IFS= read -r request; do :; done
printf 'eof\n' > "$CODEBRIDGE_TEST_ROOT/eof"
"#,
    );
    fixture.start(&["bridge", "--proxy", "http://127.0.0.1:1"]);
    fixture.wait_ready();
    let status = fixture.interrupt();
    assert_eq!(status.code(), Some(0), "{}", fixture.diagnostics());
    assert!(
        fixture.root.join("eof").exists(),
        "Codex should get a chance to exit on stdin EOF"
    );
    fixture.assert_codex_reaped();
}

#[test]
fn bridge_ctrl_c_kills_and_reaps_codex_that_ignores_stdin_eof() {
    let mut fixture = Fixture::new(
        r#"
IFS= read -r initialize || exit 1
printf 'ready\n' > "$CODEBRIDGE_TEST_ROOT/ready"
exec /bin/sleep 300
"#,
    );
    fixture.start(&["bridge", "--proxy", "http://127.0.0.1:1"]);
    fixture.wait_ready();
    let status = fixture.interrupt();
    assert_eq!(status.code(), Some(0), "{}", fixture.diagnostics());
    fixture.assert_codex_reaped();
}

#[test]
fn bridge_terminal_ctrl_c_during_initialize_exits_successfully() {
    let mut fixture = Fixture::new(
        r#"
IFS= read -r initialize || exit 1
printf 'ready\n' > "$CODEBRIDGE_TEST_ROOT/ready"
exec /bin/sleep 300
"#,
    );
    fixture.start(&["bridge", "--proxy", "http://127.0.0.1:1"]);
    fixture.wait_ready();
    // Terminal Ctrl-C signals the foreground process group, including Codex.
    // A simultaneous Codex EOF must not turn the intentional shutdown into an
    // app-server initialization error.
    let status = fixture.interrupt_target(true);
    assert_eq!(status.code(), Some(0), "{}", fixture.diagnostics());
    fixture.assert_codex_reaped();
}

#[test]
fn bridge_ctrl_c_during_telegram_startup_reaps_codex() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let mut fixture = Fixture::new(
        r#"
IFS= read -r initialize || exit 1
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r initialized || exit 1
printf 'ready\n' > "$CODEBRIDGE_TEST_ROOT/ready"
while IFS= read -r request; do :; done
printf 'eof\n' > "$CODEBRIDGE_TEST_ROOT/eof"
"#,
    );
    fixture.start(&["bridge", "--proxy", &proxy]);
    fixture.wait_ready();
    // Accept the local HTTP CONNECT but never answer it. Telegram getMe is
    // now blocked before any connection can reach Telegram's actual servers.
    let mut connection = wait_for_proxy(&listener, &mut fixture);
    connection.set_read_timeout(Some(DEADLINE)).unwrap();
    let mut request = [0_u8; 1024];
    let count = connection.read(&mut request).unwrap();
    assert!(String::from_utf8_lossy(&request[..count]).starts_with("CONNECT "));
    let status = fixture.interrupt();
    assert_eq!(status.code(), Some(0), "{}", fixture.diagnostics());
    assert!(
        fixture.root.join("eof").exists(),
        "Codex should exit naturally on stdin EOF"
    );
    fixture.assert_codex_reaped();
}

fn wait_for_proxy(listener: &TcpListener, fixture: &mut Fixture) -> TcpStream {
    let deadline = Instant::now() + DEADLINE;
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("accept Telegram proxy connection: {error}"),
        }
        fixture.assert_running();
        assert!(
            Instant::now() < deadline,
            "Telegram did not contact local proxy; {}",
            fixture.diagnostics()
        );
        thread::sleep(Duration::from_millis(10));
    }
}
