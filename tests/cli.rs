use std::process::{Command, Output, Stdio};

fn codebridge() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codebridge"));
    command
        .env_clear()
        .env("PATH", "")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .stdin(Stdio::null());
    command
}

fn run(command: &mut Command) -> Output {
    command.output().expect("codebridge should start")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "codebridge failed with {}\nstdout: {}\nstderr: {}",
        output.status,
        text(&output.stdout),
        text(&output.stderr)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "codebridge unexpectedly succeeded\nstdout: {}\nstderr: {}",
        text(&output.stdout),
        text(&output.stderr)
    );
    assert!(
        !output.stderr.is_empty(),
        "failure should explain the error"
    );
}

#[test]
fn top_level_help_documents_commands_and_global_options() {
    for arguments in [["--help"].as_slice(), ["help"].as_slice()] {
        let output = run(codebridge().args(arguments));
        assert_success(&output);
        let help = text(&output.stdout);
        for expected in ["Usage:", "codebridge", "exec", "bridge", "--cwd", "--codex"] {
            assert!(
                help.contains(expected),
                "missing {expected:?} in help: {help}"
            );
        }
        assert!(
            !help.contains("--allowed-user-id"),
            "bridge-only user option: {help}"
        );
        assert!(
            !help.contains("--proxy"),
            "bridge-only proxy option: {help}"
        );
    }
}

#[test]
fn subcommand_help_works_without_a_telegram_token() {
    for subcommand in ["exec", "bridge"] {
        for arguments in [[subcommand, "--help"], ["help", subcommand]] {
            let output = run(codebridge().args(arguments));
            assert_success(&output);
            let help = text(&output.stdout);
            for expected in ["Usage:", subcommand, "--cwd", "--codex"] {
                assert!(
                    help.contains(expected),
                    "missing {expected:?} in help: {help}"
                );
            }
            if subcommand == "exec" {
                assert!(help.to_lowercase().contains("prompt"), "exec help: {help}");
                assert!(
                    !help.contains("--allowed-user-id"),
                    "bridge-only user option: {help}"
                );
                assert!(
                    !help.contains("--proxy"),
                    "bridge-only proxy option: {help}"
                );
            } else {
                for expected in [
                    "--allowed-user-id <USER_ID>",
                    "numeric Telegram account ID",
                    "--proxy <URL>",
                    "HTTP",
                    "SOCKS",
                    "socks5h://",
                    "TELOXIDE_PROXY",
                    "HTTPS_PROXY",
                    "ALL_PROXY",
                ] {
                    assert!(
                        help.contains(expected),
                        "missing {expected:?} in bridge help: {help}"
                    );
                }
            }
        }
    }
}

#[test]
fn a_subcommand_is_required_even_when_global_options_are_present() {
    for arguments in [
        [].as_slice(),
        ["--cwd", "."].as_slice(),
        ["--codex", "codex"].as_slice(),
    ] {
        let output = run(codebridge().args(arguments));
        assert_failure(&output);
        let error = text(&output.stderr);
        assert!(
            error.contains("exec") && error.contains("bridge"),
            "missing subcommand guidance: {error}"
        );
    }
}

#[test]
fn invalid_arguments_are_rejected_with_a_diagnostic() {
    let cases: &[&[&str]] = &[
        &["unknown-command"],
        &["exec"],
        &["exec", "first prompt", "second prompt"],
        &["bridge", "unexpected-positional"],
        &["--unknown-option"],
        &["exec", "--unknown-option", "prompt"],
        &["bridge", "--unknown-option"],
        &["--proxy", "http://127.0.0.1:8080", "bridge"],
        &["exec", "--proxy", "http://127.0.0.1:8080", "prompt"],
        &["--cwd"],
        &["--codex"],
        &["exec", "--cwd"],
        &["exec", "--codex"],
    ];
    for arguments in cases {
        let output = run(codebridge().args(*arguments));
        assert_failure(&output);
        let error = text(&output.stderr).to_lowercase();
        assert!(
            error.contains("error") || error.contains("usage"),
            "arguments {arguments:?}: {error}"
        );
    }
}

#[test]
fn bridge_proxy_requires_a_value() {
    let output = run(codebridge()
        .env("TELOXIDE_TOKEN", "dummy-telegram-token")
        .args(["bridge", "--proxy"]));
    assert_failure(&output);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        text(&output.stderr).contains("--proxy"),
        "missing proxy value error: {}",
        text(&output.stderr)
    );
}

#[test]
fn bridge_rejects_missing_empty_and_whitespace_only_tokens() {
    for token in [None, Some(""), Some(" \t\r\n ")] {
        let mut command = codebridge();
        command.arg("bridge");
        if let Some(token) = token {
            command.env("TELOXIDE_TOKEN", token);
        }
        let output = run(&mut command);
        assert_failure(&output);
        assert!(
            text(&output.stderr).contains("TELOXIDE_TOKEN"),
            "token error: {}",
            text(&output.stderr)
        );
    }
}

#[cfg(unix)]
mod unix {
    use super::{assert_failure, assert_success, codebridge, run, text};
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
    const STDOUT: &str = "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"hello\"}}\nraw output: 雪\n\u{1b}[32mgreen\u{1b}[0m";
    const STDERR: &str = "fake Codex diagnostic\nsecond line without a trailing newline";
    const FAKE_CODEX: &str = r#"#!/bin/sh
printf 'started\n' > "$CODEBRIDGE_TEST_ROOT/started"
printf '%s\n' "$0" > "$CODEBRIDGE_TEST_ROOT/executable"
printf '%s\n' "$@" > "$CODEBRIDGE_TEST_ROOT/arguments"
pwd -P > "$CODEBRIDGE_TEST_ROOT/cwd"
/bin/cat > "$CODEBRIDGE_TEST_ROOT/stdin"
printf '%s' "$CODEBRIDGE_TEST_STDOUT"
printf '%s' "$CODEBRIDGE_TEST_STDERR" >&2
exit "$CODEBRIDGE_TEST_STATUS"
"#;

    struct Fixture {
        root: PathBuf,
        invocation_directory: PathBuf,
        working_directory: PathBuf,
        executable: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after 1970")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "codebridge cli {} {timestamp} {serial}",
                std::process::id()
            ));
            fs::create_dir(&root).expect("create fixture directory");
            let root = root.canonicalize().expect("canonicalize fixture directory");
            let invocation_directory = root.join("invocation directory");
            let working_directory = root.join("working directory");
            let bin_directory = root.join("bin");
            for directory in [&invocation_directory, &working_directory, &bin_directory] {
                fs::create_dir(directory).expect("create fixture subdirectory");
            }
            let executable = root.join("custom codex");
            for path in [&executable, &bin_directory.join("codex")] {
                fs::write(path, FAKE_CODEX).expect("write fake Codex executable");
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                    .expect("make fake Codex executable");
            }
            Self {
                root,
                invocation_directory,
                working_directory,
                executable,
            }
        }

        fn command(&self) -> Command {
            let mut command = codebridge();
            command
                .current_dir(&self.invocation_directory)
                .env("PATH", self.root.join("bin"))
                .env("HOME", &self.root)
                .env("CODEBRIDGE_TEST_ROOT", &self.root)
                .env("CODEBRIDGE_TEST_STDOUT", STDOUT)
                .env("CODEBRIDGE_TEST_STDERR", STDERR)
                .env("CODEBRIDGE_TEST_STATUS", "0");
            command
        }

        fn assert_execution(&self, prompt: &str, expected_directory: &std::path::Path) {
            assert_eq!(
                fs::read(self.root.join("arguments")).expect("read Codex arguments"),
                b"exec\n--json\n--color\nnever\n--ephemeral\n-\n"
            );
            assert_eq!(
                fs::read(self.root.join("stdin")).expect("read Codex stdin"),
                prompt.as_bytes()
            );
            let directory =
                fs::read_to_string(self.root.join("cwd")).expect("read Codex working directory");
            assert_eq!(
                directory.trim_end_matches('\n'),
                expected_directory
                    .to_str()
                    .expect("fixture path should be UTF-8")
            );
        }

        fn assert_executable(&self, expected_executable: &std::path::Path) {
            let executable = fs::read_to_string(self.root.join("executable"))
                .expect("read Codex executable path");
            assert_eq!(
                executable.trim_end_matches('\n'),
                expected_executable
                    .to_str()
                    .expect("fixture path should be UTF-8")
            );
        }

        fn assert_not_started(&self) {
            assert!(
                !self.root.join("started").exists(),
                "Codex must not start for a rejected command"
            );
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn exec_without_a_telegram_token_forwards_defaults_prompt_and_raw_output() {
        let fixture = Fixture::new();
        let prompt = "\n  Explain --cwd literally: 雪\nkeep this trailing whitespace \t\n";
        let output = run(fixture.command().args(["exec", prompt]));
        assert_success(&output);
        assert_eq!(output.stdout, STDOUT.as_bytes());
        assert_eq!(output.stderr, STDERR.as_bytes());
        fixture.assert_execution(prompt, &fixture.invocation_directory);
        fixture.assert_executable(&fixture.root.join("bin/codex"));
    }

    #[test]
    fn debug_logging_uses_stderr_and_keeps_prompt_and_credentials_private() {
        let fixture = Fixture::new();
        let prompt = "private-prompt-sentinel-8f31";
        let token = "private-token-sentinel-8f31";
        let proxy_user = "private-proxy-user-8f31";
        let proxy_password = "private-proxy-password-8f31";
        let output = run(fixture
            .command()
            .env("RUST_LOG", "codebridge=debug")
            .env("TELOXIDE_TOKEN", token)
            .env(
                "TELOXIDE_PROXY",
                format!("http://{proxy_user}:{proxy_password}@127.0.0.1:8080"),
            )
            .args(["exec", prompt]));
        assert_success(&output);
        assert_eq!(output.stdout, STDOUT.as_bytes());
        let stderr = text(&output.stderr);
        for expected in [
            STDERR,
            "DEBUG",
            "Starting one-off task",
            "One-off task completed",
            "exit_code=0",
        ] {
            assert!(stderr.contains(expected), "missing {expected:?}: {stderr}");
        }
        assert!(!stderr.contains('\u{1b}'), "ANSI in diagnostics: {stderr}");
        for secret in [prompt, token, proxy_user, proxy_password] {
            assert!(
                !stderr.contains(secret),
                "stderr exposed {secret}: {stderr}"
            );
            assert!(
                !text(&output.stdout).contains(secret),
                "stdout exposed {secret}"
            );
        }
        fixture.assert_execution(prompt, &fixture.invocation_directory);
    }

    #[test]
    fn invalid_log_filter_silently_falls_back_to_default_logging() {
        let fixture = Fixture::new();
        let filter = "codebridge=definitely-not-a-level";
        let output = run(fixture
            .command()
            .env("RUST_LOG", filter)
            .args(["exec", "prompt"]));
        assert_success(&output);
        assert_eq!(output.stdout, STDOUT.as_bytes());
        assert_eq!(output.stderr, STDERR.as_bytes());

        let output = run(fixture
            .command()
            .env("RUST_LOG", filter)
            .arg("--codex")
            .arg(fixture.root.join("missing codex"))
            .args(["exec", "prompt"]));
        assert_failure(&output);
        assert!(output.stdout.is_empty());
        let stderr = text(&output.stderr);
        assert!(stderr.contains("ERROR"), "missing error event: {stderr}");
        assert!(!stderr.contains(filter), "filter warning: {stderr}");
        assert!(!stderr.contains("DEBUG"), "debug logging enabled: {stderr}");
        assert!(!stderr.contains('\u{1b}'), "ANSI in diagnostics: {stderr}");
    }

    #[test]
    fn log_filter_off_suppresses_application_errors() {
        let fixture = Fixture::new();
        let output = run(fixture
            .command()
            .env("RUST_LOG", "off")
            .arg("--codex")
            .arg(fixture.root.join("missing codex"))
            .args(["exec", "prompt"]));
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
        fixture.assert_not_started();
    }

    #[test]
    fn log_filter_off_preserves_child_output_and_nonzero_status() {
        let fixture = Fixture::new();
        let prompt = "produce partial output then fail";
        let output = run(fixture
            .command()
            .env("RUST_LOG", "off")
            .env("CODEBRIDGE_TEST_STATUS", "23")
            .args(["exec", prompt]));
        assert_eq!(output.status.code(), Some(23));
        assert_eq!(output.stdout, STDOUT.as_bytes());
        assert_eq!(output.stderr, STDERR.as_bytes());
        fixture.assert_execution(prompt, &fixture.invocation_directory);
    }

    #[test]
    fn exec_ignores_invalid_telegram_proxy_configuration() {
        let fixture = Fixture::new();
        let prompt = "run without Telegram";
        let output = run(fixture
            .command()
            .env(
                "TELOXIDE_PROXY",
                "http://telegram-user:telegram-password@127.0.0.1:invalid",
            )
            .args(["exec", prompt]));
        assert_success(&output);
        assert_eq!(output.stdout, STDOUT.as_bytes());
        assert_eq!(output.stderr, STDERR.as_bytes());
        fixture.assert_execution(prompt, &fixture.invocation_directory);
    }

    #[test]
    fn global_paths_work_before_after_and_across_the_subcommand() {
        for placement in 0..4 {
            let fixture = Fixture::new();
            let prompt = "a single quoted prompt with spaces";
            let mut command = fixture.command();
            match placement {
                0 => {
                    command.arg("--codex").arg(&fixture.executable).args([
                        "--cwd",
                        "../working directory",
                        "exec",
                        prompt,
                    ]);
                }
                1 => {
                    command
                        .args(["exec", prompt, "--codex"])
                        .arg(&fixture.executable)
                        .args(["--cwd", "../working directory"]);
                }
                2 => {
                    command.arg("--codex").arg(&fixture.executable).args([
                        "exec",
                        prompt,
                        "--cwd",
                        "../working directory",
                    ]);
                }
                3 => {
                    command
                        .args(["--cwd", "../working directory", "exec", "--codex"])
                        .arg(&fixture.executable)
                        .arg(prompt);
                }
                _ => unreachable!(),
            }
            let output = run(&mut command);
            assert_success(&output);
            assert_eq!(output.stdout, STDOUT.as_bytes());
            assert_eq!(output.stderr, STDERR.as_bytes());
            fixture.assert_execution(prompt, &fixture.working_directory);
            fixture.assert_executable(&fixture.executable);
        }
    }

    #[test]
    fn exec_preserves_nonzero_codex_status_and_both_output_streams() {
        let fixture = Fixture::new();
        let prompt = "produce partial output then fail";
        let output = run(fixture
            .command()
            .env("CODEBRIDGE_TEST_STATUS", "23")
            .args(["exec", prompt]));
        assert_eq!(output.status.code(), Some(23));
        assert_eq!(output.stdout, STDOUT.as_bytes());
        assert_eq!(output.stderr, STDERR.as_bytes());
        fixture.assert_execution(prompt, &fixture.invocation_directory);
    }

    #[test]
    fn exec_allows_empty_successful_output() {
        let fixture = Fixture::new();
        let output = run(fixture
            .command()
            .env("CODEBRIDGE_TEST_STDOUT", "")
            .env("CODEBRIDGE_TEST_STDERR", "")
            .args(["exec", "quiet prompt"]));
        assert_success(&output);
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        fixture.assert_execution("quiet prompt", &fixture.invocation_directory);
    }

    #[test]
    fn an_option_terminator_allows_a_prompt_starting_with_a_hyphen() {
        let fixture = Fixture::new();
        let prompt = "--cwd is part of this prompt";
        let output = run(fixture.command().args(["exec", "--", prompt]));
        assert_success(&output);
        fixture.assert_execution(prompt, &fixture.invocation_directory);
    }

    #[test]
    fn blank_prompts_fail_before_starting_codex() {
        for prompt in ["", "   ", "\t\r\n"] {
            let fixture = Fixture::new();
            let output = run(fixture.command().args(["exec", prompt]));
            assert_failure(&output);
            assert!(
                text(&output.stderr).to_lowercase().contains("prompt"),
                "prompt error: {}",
                text(&output.stderr)
            );
            fixture.assert_not_started();
        }
    }

    #[test]
    fn extra_positionals_and_unknown_options_fail_before_starting_codex() {
        for arguments in [
            ["exec", "first prompt", "second prompt"],
            ["exec", "prompt", "--unknown-option"],
        ] {
            let fixture = Fixture::new();
            let output = run(fixture.command().args(arguments));
            assert_failure(&output);
            fixture.assert_not_started();
        }
    }

    #[test]
    fn a_missing_codex_executable_fails_clearly() {
        let fixture = Fixture::new();
        let missing_executable = fixture.root.join("missing codex");
        let output = run(fixture
            .command()
            .arg("--codex")
            .arg(missing_executable)
            .args(["exec", "prompt"]));
        assert_failure(&output);
        assert!(output.stdout.is_empty());
        fixture.assert_not_started();
    }

    #[test]
    fn an_invalid_working_directory_fails_before_starting_codex() {
        let fixture = Fixture::new();
        let output = run(fixture
            .command()
            .args(["exec", "prompt", "--cwd"])
            .arg(fixture.root.join("missing directory")));
        assert_failure(&output);
        assert!(output.stdout.is_empty());
        fixture.assert_not_started();
    }

    #[test]
    fn invalid_user_restrictions_fail_before_starting_codex() {
        let cases: &[&[&str]] = &[
            &["bridge", "--allowed-user-id"],
            &["bridge", "--allowed-user-id", ""],
            &["bridge", "--allowed-user-id", "0"],
            &["bridge", "--allowed-user-id", "-1"],
            &["bridge", "--allowed-user-id=-1"],
            &["bridge", "--allowed-user-id", "not-a-number"],
            &["bridge", "--allowed-user-id", "18446744073709551616"],
            &["bridge", "--allowed-user-id", "@username"],
            &["--allowed-user-id", "123", "bridge"],
            &["exec", "prompt", "--allowed-user-id", "123"],
        ];
        for arguments in cases {
            let fixture = Fixture::new();
            let output = run(fixture.command().args(*arguments));
            assert_failure(&output);
            assert_eq!(output.status.code(), Some(2), "arguments: {arguments:?}");
            fixture.assert_not_started();
        }
    }

    #[test]
    fn valid_user_restrictions_reach_bridge_startup() {
        for user_id in ["1", "4294967296", "18446744073709551615"] {
            let fixture = Fixture::new();
            let output = run(fixture
                .command()
                .args(["bridge", "--allowed-user-id", user_id]));
            assert_failure(&output);
            assert_eq!(output.status.code(), Some(1));
            assert!(text(&output.stderr).contains("TELOXIDE_TOKEN"));
            fixture.assert_not_started();
        }
    }

    #[test]
    fn bridge_checks_missing_or_blank_tokens_before_starting_codex() {
        for token in [None, Some(""), Some(" \t\r\n ")] {
            let fixture = Fixture::new();
            let mut command = fixture.command();
            command
                .env("TELOXIDE_PROXY", "invalid proxy")
                .args(["bridge", "--proxy", "invalid proxy", "--codex"])
                .arg(&fixture.executable);
            if let Some(token) = token {
                command.env("TELOXIDE_TOKEN", token);
            }
            let output = run(&mut command);
            assert_failure(&output);
            assert!(
                text(&output.stderr).contains("TELOXIDE_TOKEN"),
                "token error: {}",
                text(&output.stderr)
            );
            fixture.assert_not_started();
        }
    }

    #[test]
    fn bridge_rejects_invalid_proxy_configuration_before_starting_codex() {
        let invalid_proxies = [
            "http://telegram-user:telegram-password@127.0.0.1:invalid",
            "socks5h://telegram-user:telegram-password@127.0.0.1:invalid",
            "unsupported://telegram-user:telegram-password@127.0.0.1:8080",
            "",
        ];
        for proxy in invalid_proxies {
            for explicit in [false, true] {
                let fixture = Fixture::new();
                let mut command = fixture.command();
                command
                    .env("TELOXIDE_TOKEN", "dummy-telegram-token")
                    .arg("bridge");
                if explicit {
                    command
                        .env("TELOXIDE_PROXY", "http://127.0.0.1:8080")
                        .args(["--proxy", proxy]);
                } else {
                    command.env("TELOXIDE_PROXY", proxy);
                }
                let output = run(&mut command);
                assert_failure(&output);
                let error = text(&output.stderr);
                assert!(
                    error.to_lowercase().contains("proxy"),
                    "proxy error: {error}"
                );
                for secret in ["telegram-user", "telegram-password", "dummy-telegram-token"] {
                    assert!(
                        !error.contains(secret),
                        "proxy error exposed {secret}: {error}"
                    );
                    assert!(
                        !text(&output.stdout).contains(secret),
                        "stdout exposed {secret}"
                    );
                }
                fixture.assert_not_started();
            }
        }
    }

    #[test]
    fn an_explicit_proxy_overrides_invalid_telegram_proxy_environment() {
        for proxy in ["http://127.0.0.1:8080", "socks5h://127.0.0.1:1080"] {
            let fixture = Fixture::new();
            let output = run(fixture
                .command()
                .env("TELOXIDE_TOKEN", "dummy-telegram-token")
                .env(
                    "TELOXIDE_PROXY",
                    "http://ignored-user:ignored-password@127.0.0.1:invalid",
                )
                .args(["bridge", "--proxy", proxy, "--codex"])
                .arg(fixture.root.join("missing codex")));
            assert_failure(&output);
            let error = text(&output.stderr);
            assert!(
                error.contains("failed to start Codex app-server"),
                "valid explicit proxy should reach Codex startup: {error}"
            );
            for secret in ["ignored-user", "ignored-password", "dummy-telegram-token"] {
                assert!(
                    !error.contains(secret),
                    "startup error exposed {secret}: {error}"
                );
            }
            fixture.assert_not_started();
        }
    }
}
