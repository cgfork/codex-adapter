# Core bot and code-agent bridge

The `codebridge-core` package lives in `crates/core`; its Rust import name is
`codebridge_core`. This avoids conflicting with Rust's standard `core` crate.
It provides the reusable workflow and types for connecting a bot to a code agent.

## Interfaces

| Interface | Responsibility |
| --- | --- |
| `Bot` | Poll incoming messages, send normalized agent output and notices, present approval prompts, and remove approval controls. |
| `CodeAgent` | Manage sessions/turns, emit normalized events, answer approvals, reject unsupported requests, execute independent tasks, and shut down. |
| `Store` | Load and save each chat's ordered session history. |
| `Session` | Read a history through the store's generic associated `Session<'a>` type. |
| `Memory<C>` | Store session histories in memory, shared across clones. |
| `Bridge<B, A, S>` | Connect those interfaces, route conversations, enforce access and approval ownership, and coordinate delivery and cancellation. The store defaults to `Memory<B::ChatId>`. |

`Bot` has associated `ChatId`, `UserId`, and `MessageId` types. They must support
cloning, equality, hashing, and use across async tasks; they can be strings,
integers, or a provider's identifier types. The core never parses them.

`IncomingMessage` carries a message or approval decision. Bot implementations
classify supported commands into `Command` and identify the sender only when a
human is acting as themselves. Use `sender: None` for anonymous, bot, or
on-behalf-of-chat senders. This identity contract is required for allowlists and
approval ownership; callbacks with no accessible originating message must not
be turned into approval decisions.

`AgentEvent` describes output, completed turns, approval requests, resolved
requests, and unsupported requests. Session, turn, and request IDs are opaque
strings. Adapters preserve their identity and translate them to their native
protocol. `AgentOutput` describes messages, plans, command results, file changes,
and errors. `ExecutionOutput` also carries independent-task status and stderr,
so a failure with empty agent output can still be reported.

## Compose the existing adapters

The CLI uses the equivalent of this composition, with cancellation also applied
while the Codex app-server is starting:

```rust,no_run
use codebridge_core::{Bridge, BridgeResult};
use codexbridge::{AppServer, Codex, CodexAgent};
use teloxide::types::UserId;
use tgbot::TelegramBot;

async fn run(token: String) -> BridgeResult<()> {
    let bot = TelegramBot::new(tgbot::create_bot(token, None)?);
    let codex = Codex::new(std::env::current_dir()?);
    let server = AppServer::start(&codex).await?;
    let agent = CodexAgent::new(codex, server);

    Bridge::new(bot, agent)
        .with_allowed_user_id(Some(UserId(123456789)))
        .run(async { tokio::signal::ctrl_c().await })
        .await
}
```

Construct and configure transports before handing them to the core. The core
does not read bot tokens, proxy settings, working directories, or Codex config.
Omit `with_allowed_user_id`, or pass `None`, to accept messages from all users.
Approval decisions always require the initiating user, even without an allowlist.

## Session storage

`codebridge_core::session::Store` is an async interface with `load`, `list`,
`save`, and `remove` operations. Its associated `ChatId` must match the bot's
chat identifier type. Its session is a generic associated type:

```rust,ignore
type Session<'a>: Session + Send + 'a where Self: 'a;
```

`load` and `list` return these handles, so implementations can expose borrowed
views, lock guards, or owned snapshots. The `Session` trait provides ordered
`session_ids`, `active_session`, and `to_state` access. `SessionState` is an owned
snapshot used for updates through `save`; its final ID is active. The traits,
state, and `Memory` are also reexported from the crate root.

`Bridge::new` creates an empty `Memory` store. Use `with_store` to supply another
implementation, or retain a clone of `Memory` to inspect and reuse histories:

```rust,no_run
use codebridge_core::{Bot, Bridge, BridgeResult, CodeAgent, Memory, Store};

async fn run<B: Bot, A: CodeAgent>(bot: B, agent: A) -> BridgeResult<()> {
    let sessions = Memory::<B::ChatId>::default();
    Bridge::new(bot, agent)
        .with_store(sessions.clone())
        .run(async { tokio::signal::ctrl_c().await })
        .await?;
    let _saved_histories = sessions.list().await?;
    Ok(())
}
```

`Memory` defines `Session<'a> = SessionState`: its clones share writes, while
reads return independent snapshots without holding a lock. Other stores may
borrow from `self` for the handle's lifetime. Memory
lasts until its last clone is dropped; it does not survive a process restart.
Implement `Store` with a file or database backend for durable histories.

The bridge reads `list` once at startup and saves each session creation or
deletion before acknowledging it. Failed storage operations stop the bridge;
they are never treated as recoverable agent rejections. A failed write must leave
the previous record intact. Use one active bridge writer per store; external
edits are loaded on the next bridge run. Store futures may be cancelled during
shutdown, so writes must remain atomic even if the caller stops awaiting them.
Each session ID must belong to exactly one chat, and `list` returns at most one
record per chat. Scope a store to the same bot account and agent backend.

Active turns, approval ownership/buttons, and independent jobs belong to the
running bridge and are never restored from storage. Agent implementations must
accept stored IDs in `start_turn`, loading or resuming them as necessary. The
Codex adapter resumes stored threads on their first use on a new connection,
following the [Codex app-server lifecycle](https://learn.chatgpt.com/docs/app-server).

## Workflow and failure handling

Each chat has an independent session list, one active conversation turn, and at
most one independent task. The shared bridge implements initialization, new and
deleted sessions, listing, interruption, and independent execution. A deleted
session is removed from the configured store; the agent's conversation files
are retained.

Approvals bind the request to its chat, delivered message, initiating user,
session, and turn. Completion, interruption, resolution, and failed delivery
invalidate approvals. Unsupported agent requests are explicitly rejected.

Delivery runs in a FIFO queue for each chat. A slow chat does not block agent
events, other chats, or cancellation. On shutdown, the bridge stops the bot,
cancels independent tasks, invalidates approvals, and shuts down the agent before
draining deliveries for up to one second.

An agent's `is_connection_fatal` method distinguishes recoverable request
rejections from failures that make continued operation unsafe. Its default is
fatal. Unexpected event-stream closure ends the bridge; a normal bot-loop return
ends it gracefully. Provider errors retain their sources through `BridgeError`.

## Cancellation contract

- Dropping `Bot::run` must stop polling, including in-flight network operations.
- Bot delivery methods may be cancelled and may run concurrently across chats.
- `CodeAgent::next_event` must preserve pending events when its future is dropped.
- An in-flight agent operation may be cancelled by shutdown; the bridge then
  calls `CodeAgent::shutdown` and does not reuse that connection.
- `CodeAgent::exec` returns an owned, `Send + 'static` future so a task can run
  alongside session events. Dropping that future must stop its work and arrange
  for any subprocesses to be reaped.

Adapter tests should verify conversion and transport behavior. Test shared
workflow with fake implementations, without credentials or external services:

```sh
cargo test -p codebridge-core
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```
