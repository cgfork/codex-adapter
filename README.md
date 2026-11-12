# Codebridge

A Rust CLI that runs one-off Codex tasks or connects Telegram chats to a Codex
app-server. Requires Rust 1.85 or newer, Cargo, and an installed, authenticated
Codex executable. Codebridge uses the existing Codex configuration and credentials.

## Install and run

Build from the repository root:

```sh
cargo build --release -p codebridge
./target/release/codebridge --help
```

To install the executable on `PATH` using the checked-in dependency versions:

```sh
cargo install --path . --locked
```

The CLI requires one of two subcommands:

```text
codebridge [--cwd <PATH>] [--codex <PATH>] exec <PROMPT>
codebridge [--cwd <PATH>] [--codex <PATH>] bridge [--proxy <URL>] [--allowed-user-id <USER_ID>]
```

`--cwd` (also `-C`) defaults to `.` and is resolved against Codebridge's startup
directory before Codex starts. `--codex` defaults to `codex` on `PATH`; use an
absolute path to select a particular installation. Both options work before or
after the subcommand. `--help` and `--version` are available without starting Codex.

### Codex project trust

If Codex reports that project-local config, hooks, and exec policies are disabled,
add or update the project's entry in your user-level `~/.codex/config.toml`
(`$CODEX_HOME/config.toml` when `CODEX_HOME` is set):

```toml
[projects."/absolute/path/to/project"]
trust_level = "trusted"
```

Use the absolute project path shown in the diagnostic. Update an existing entry
instead of adding a duplicate table, then restart the bridge. Trust enables that
project's local configuration, hooks, and execution policies; set it for projects
you trust. See the [official Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).

## One-off execution

Pass the entire prompt as one quoted argument:

```sh
cargo run -p codebridge -- exec "Explain the current project"
./target/release/codebridge -C /path/to/project exec "Run the tests"
./target/release/codebridge exec "Review this project" --codex /path/to/codex
```

The prompt must contain non-whitespace text. Codebridge invokes
`codex exec --json --color never --ephemeral -`, writes the prompt to stdin, and
forwards the collected stdout and stderr unchanged to the corresponding terminal
streams. Output appears after the subprocess exits; it is not streamed live.
Codebridge returns the subprocess exit code, or `128 + signal` on Unix when
terminated by a signal. Invalid CLI arguments exit with code 2; startup and
transport failures produce an error on stderr and exit with code 1.
Ctrl-C cancels the task, terminates and waits for the Codex subprocess, and exits
with code 130. Buffered task output is discarded when cancelled.

This mode does not use Telegram and does not require `TELOXIDE_TOKEN`.

## Telegram bridge

Set the bot token in the environment and start the bridge:

```sh
export TELOXIDE_TOKEN='your-telegram-bot-token'
cargo run -p codebridge -- --cwd /path/to/project bridge
```

To accept input only from your Telegram account, supply its positive numeric
user ID with the bridge-only `--allowed-user-id` option:

```sh
cargo run -p codebridge -- --cwd /path/to/project bridge --allowed-user-id 123456789
```

Use the account's Telegram `from.id`, not its `@username`, the bot's ID, or a chat
ID. To find it, temporarily run `cargo run -p tgbot` with the bot token set, send
the bot a private message, and inspect that message's `from` user `id` in the
printed event. Stop the receiver before starting the bridge so they do not poll
the same bot simultaneously.

When set, this option accepts commands, text prompts, `/exec`, interruptions, and
approval button clicks only from that account. Messages without an identifiable
human sender, including anonymous administrator and channel posts, are ignored.
Other accounts' updates are silently ignored; Telegram may still acknowledge
button clicks. Approval decisions also retain the existing turn-owner checks.
The account can use private or group chats; replies remain visible in the
originating chat, including to other group members. Omit the option to preserve
access for all users.

`bridge` requires a nonempty `TELOXIDE_TOKEN`. It validates Telegram proxy
configuration before launching `codex app-server`, initializes the connection,
and polls Telegram. Ctrl-C works during startup, Telegram polling, and Codex RPC
requests. It stops Telegram polling, cancels one-off jobs, invalidates
pending approvals, and closes the app-server's input so it can exit. Codebridge
waits up to one second for the app-server, then terminates and waits for it if
necessary. A successful Ctrl-C shutdown exits with code 0.

Use the bridge-only `--proxy` option to send Telegram requests through an HTTP
or SOCKS proxy:

```sh
cargo run -p codebridge -- bridge --proxy http://127.0.0.1:8080
cargo run -p codebridge -- bridge --proxy socks5h://127.0.0.1:1080
TELOXIDE_PROXY=http://127.0.0.1:8080 cargo run -p codebridge -- bridge
```

`socks5h://` resolves Telegram hostnames through the proxy. Proxy selection is
`--proxy` first, then `TELOXIDE_PROXY`, then the standard `HTTPS_PROXY`/`ALL_PROXY`
environment settings handled by reqwest. The selected client is shared by
Telegram polling, bot identity lookup, command registration, replies, and
approval-button updates. Invalid proxy configuration fails before Codex starts;
proxy errors omit proxy credentials.

An explicit `--proxy` or `TELOXIDE_PROXY` takes precedence over the standard
proxy environment, including `NO_PROXY` exclusions. When neither is set,
reqwest keeps its standard environment proxy and `NO_PROXY` behavior.

`--proxy` and `TELOXIDE_PROXY` configure Telegram requests only; `codebridge exec`
does not read `TELOXIDE_PROXY`. Standard proxy environment variables are inherited
by the Codex subprocess and can also affect its own network requests. Set
`TELOXIDE_PROXY` when only Telegram should use the proxy.

Each Telegram chat has its own active Codex thread and at most one running
conversation turn. The first text prompt creates a session automatically.
Completed agent messages, plans, commands, file changes, and errors are rendered
and sent to the originating chat through `TelegramBot`; incremental deltas and
protocol bookkeeping are omitted. Long replies are split to fit Telegram limits.

| Telegram input | Behavior |
| --- | --- |
| `/init` | Create a session if needed, or show the active session ID. |
| `/new` | Create a fresh active thread, retaining earlier IDs in the session list. A running turn must finish or be interrupted first. |
| `/delete` | Remove the active session from this chat's list and make the most recent remaining session active. A running turn must finish or be interrupted first. |
| `/sessions` | List this chat's session IDs and mark the active one. |
| `/interrupt` | Interrupt this chat's active turn and cancel any running one-off task. |
| `/exec <prompt>` | Run an independent, ephemeral Codex task without changing the active session. |
| Ordinary text | Start a turn in the active session. Wait for its completion before submitting another prompt. |

One-off `/exec` tasks run separately from the app-server event loop, so other
chats, server events, and interruption remain available while they execute.
Each chat can run at most one one-off task at a time. Failed tasks report their
exit status and stderr even if Codex produced no JSONL output.

Telegram sends and button edits use a separate FIFO outbox for each chat, so a
slow Telegram request does not block Codex events, incoming commands, or other
chats. Shutdown terminates Codex before allowing up to one second for queued
Telegram deliveries, then cancels and waits for unfinished sends. Delivery errors are logged;
shutdown notices are best effort. A Codex RPC transport timeout or invalid
response stops the bridge to avoid continuing with an uncertain running turn;
explicit server RPC rejections are reported to the chat without disconnecting.

Command and file-change approval requests show Allow and Deny buttons. A decision
applies only to that exact pending request, its Telegram chat and message, and its
current Codex thread and turn. Only the Telegram user who started the turn may
decide. Completed, interrupted, resolved, or already answered requests cannot be
authorized by an old button. Requests that cannot be associated with an initiating
user and turn are declined. Unsupported server requests receive an explicit
protocol rejection and are reported when their chat is known.

Sessions and pending approvals are kept in memory and reset on restart. `/delete`
removes only the current session from the bridge's in-memory list; saved Codex
thread files are retained. If no sessions remain, `/init` or the next text prompt
creates a fresh session. Deleting without an active session reports that there
is nothing to delete. Independent `/exec` tasks are unaffected. The session list
does not offer a command to select an earlier thread; deleting the current
session reactivates the most recent remaining one. Group members share their
chat's session. By default, any member can submit prompts or interrupt it;
`--allowed-user-id` restricts those actions to the selected account. There is no
separate chat allowlist. Use a bot whose access matches the intended audience
and the permissions of the Codex process. Media prompts and interactive server
requests other than command/file approvals are not supported. One-off `/exec` uses the
noninteractive exec transport and does not offer Telegram approval buttons.

## Logging

The `codebridge` and standalone `tgbot` binaries initialize `tracing` and write
timestamped logs to stderr, with levels, module targets, and fields such as chat,
thread, turn, and RPC IDs. Libraries emit events through the caller's subscriber.
The default filter is `warn,codebridge=info,codebridge_core=info,tgbot=info,codexbridge=info`: application
lifecycle events are visible, while dependencies log warnings and errors.

Set `RUST_LOG` to override the filter, for example:

```sh
RUST_LOG=warn,codebridge=debug,codebridge_core=debug,codexbridge=debug,tgbot=debug cargo run -p codebridge -- bridge
RUST_LOG=error cargo run -p tgbot
RUST_LOG=off ./target/release/codebridge exec "Explain the project"
```

Missing or invalid filters use the default. Normal `codebridge exec` lifecycle
events use debug level, preserving its raw stdout and stderr forwarding by
default. `RUST_LOG=off` disables application logs, including error diagnostics;
Codex subprocess output and exit codes are still forwarded. The standalone
receiver continues to print incoming events to stdout. Lifecycle events use
metadata without logging prompt text, message bodies, bot tokens, or proxy URLs.

## Workspace and development

- `src/`: CLI and composition of the Telegram and Codex adapters.
- `crates/core/`: bot/agent traits, session stores, shared events, and the generic session/approval bridge.
- `crates/codexbridge/`: Codex transports, event normalization, and the `CodeAgent` implementation.
- `crates/tgbot/`: Telegram transport, message classification/rendering, and the `Bot` implementation.
- `tests/`: process-level Codebridge CLI tests.

Run from the repository root:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

The workspace uses Rust's built-in test framework, Rustfmt (`rustfmt.toml`), and
Clippy. Install the development tools with `rustup component add rustfmt clippy`.
To format just the Codebridge implementation, run:

```sh
rustfmt --edition 2024 src/main.rs src/bridge.rs
```

Keep `Cargo.lock` under version control for reproducible executable builds. Never
commit bot tokens or private Codex configuration.

The workspace patches `proc-macro-error2` 2.0.1 with a vendored copy containing the
`proc_macro` visibility fix proposed in [upstream PR #14](https://github.com/GnomedDev/proc-macro-error-2/pull/14).
This resolves Rust's future-incompatibility warning without suppressing it. The
vendored crate's `PATCH.md` records its origin and the change.

The standalone Telegram event receiver remains available as `cargo run -p tgbot`;
it also requires `TELOXIDE_TOKEN`. Library callers can use `tgbot::run(bot, sender)`
and `tgbot::send(bot, message)` independently of the Codebridge CLI.

## Core library

`crates/core` is named `codebridge-core` within this workspace and imported
as `codebridge_core`. It defines `Bot` and `CodeAgent`, normalized incoming events
and agent output, and `Bridge<B, A, S>` for composing any implementations of those
traits. The core depends on Tokio and tracing; it contains no Telegram SDK types,
Codex wire messages, credentials, or process-launch configuration.

The CLI composes `tgbot::TelegramBot` and `codexbridge::CodexAgent` through this
bridge. Session management, independent tasks, access checks, approvals, ordered
chat deliveries, and shutdown all run in the shared implementation. Existing
standalone Telegram and raw Codex APIs remain available.

Session history uses an async `Store` trait with a generic associated
`Session<'a>` type for borrowed or owned history handles. `Bridge::new` defaults to `Memory`,
whose clones share histories; `.with_store(store)` accepts another backend.
The bridge restores saved session IDs at startup and persists changes before
acknowledging them. Active turns and approvals remain scoped to the current run.
The CLI uses the default memory store, so its session list resets on restart.

See [the core API guide](docs/core.md) for the adapter contracts, a composition
example, and cancellation requirements. Run the provider-independent tests with
`cargo test -p codebridge-core`.

## Telegram library

Set `TELOXIDE_TOKEN`, then run `cargo run -p tgbot` to poll Telegram and print
incoming events without starting Codex. The library exposes
`tgbot::run(bot, sender)` to forward those events through a bounded Tokio channel
to a consuming application.

The standalone receiver also honors `TELOXIDE_PROXY`, with the same standard
proxy environment fallback:

```sh
TELOXIDE_PROXY=socks5h://127.0.0.1:1080 cargo run -p tgbot
```

Library callers can use `tgbot::create_bot(token, proxy)` to build a bot with the
same configuration. Pass `Some("http://127.0.0.1:8080")` for an explicit proxy or
`None` to use `TELOXIDE_PROXY` and then reqwest's standard proxy settings. The
function returns a result so invalid configuration can be reported before
starting other processes. Pass the resulting bot and its clones to
`tgbot::run`, `tgbot::send`, and other Telegram operations to retain the selected
proxy.

`IncomingMessage` distinguishes three kinds of events:

- `Raw`: ordinary text, media, and unrecognized commands, preserving the Telegram message.
- `Command`: `/sessions`, `/interrupt`, `/init`, `/new`, `/delete`, or `/exec <prompt>`, preserving the command and message.
- `ApprovalSignal`: `Allow` or `Deny`, preserving the Telegram callback query.

Commands addressed to this bot, such as `/sessions@your_bot`, are also recognized.
Approval signals come from inline buttons with callback data `allow` or `deny`;
use `ApprovalSignal::callback_data()` when constructing those buttons. Plain text
`allow` or `deny` remains a raw message. Callback queries are acknowledged, and
unrecognized callback data is ignored. Commands are registered with Telegram at
startup; session behavior and approval authorization belong to the consuming
application.

Run the message classification tests with `cargo test -p tgbot --test messages`.

## Agent output library

`OutgoingMessage` wraps the agent's JSONL stdout together with its destination
Telegram chat ID. Pass complete JSONL records and the originating chat ID to the
send function:

```rust
let message = tgbot::OutgoingMessage::from_stdout(chat_id, stdout);
tgbot::send(&bot, &message).await?;
```

`codexbridge` recognizes completed items from both `codex exec --json` and the
app-server protocol. Agent replies and plans are rendered from Markdown into
Telegram HTML, including bold headings, bold and italic text, strikethrough,
links, lists, quotations, and code blocks. Command results use literal code
blocks; file changes and errors get labeled messages. Raw JSON, protocol
bookkeeping, and incremental deltas are omitted; completed items supply the
authoritative text.

Long messages are split at a maximum of 4096 UTF-16 code units of visible text.
Every chunk has balanced HTML tags, intact entities, and complete Unicode
characters. HTML from agent text is escaped. Empty output sends no messages.
Invalid JSONL reports its line number before anything is sent, and Telegram
send errors propagate to the library caller. Codebridge handles transport,
output buffering, and ordered delivery separately.

Use `tgbot::render_jsonl(&stdout)` to inspect the resulting HTML without sending
Telegram messages. Test normalization with `cargo test -p codexbridge` and
formatting with `cargo test -p tgbot --lib`.
