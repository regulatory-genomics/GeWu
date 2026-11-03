# gewu-client

Async Rust client library for a GeWu server (`gewu-server serve`). It also
contains the UI-agnostic conversation model shared by the terminal UI
(`gewu-tui`) and the desktop UI (`gewu-gui`).

For the terminal front-end, see [`gewu-tui`](../gewu-tui/README.md); for the
desktop one, see [`gewu-gui`](../gewu-gui/README.md).

## Library usage

Start a server first:

```bash
cargo run -p gewu-server -- serve
```

Then talk to it from Rust:

```rust
use gewu_client::Client;
use gewu_protocol::{AgentEvent, ServerMessage};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Client::tcp("127.0.0.1", 8421).await?;
    // or: Client::websocket("ws://127.0.0.1:8420").await?;

    client.ping().await?;
    println!("tools: {:?}", client.list_tools().await?);

    // Stream an ask, printing content deltas as they arrive.
    let mut ask = client.ask("Summarize this repository").send().await?;
    while let Some(frame) = ask.next_frame().await {
        match frame? {
            ServerMessage::Event { event, .. } => {
                if let Ok(AgentEvent::ContentDelta { delta }) =
                    serde_json::from_value::<AgentEvent>(event)
                {
                    print!("{delta}");
                }
            }
            ServerMessage::Result { .. } => break,
            ServerMessage::Error { error, .. } => anyhow::bail!("{error}"),
            _ => {}
        }
    }

    client.close().await;
    Ok(())
}
```

When you only need the final text:

```rust
let response = client.ask("What is 2+2?").send().await?.final_response().await?;
```

## API

| Item | Description |
|------|-------------|
| `Client::tcp(host, port)` / `Client::websocket(url)` | Connect; returns a clonable handle sharing one connection. |
| `client.ask(prompt)` | Builder; `.send().await?` starts the ask and returns an `AskStream`. Optional `.session_id(...)` / `.agent(...)` (honored when the connection's session is first created). |
| `AskStream::next_frame()` | Next protocol frame; ends with a terminal `result` (success) or `error` (failure), then returns `None`. An aborted (interrupted) ask ends with `Err(Error::Interrupted)` instead. |
| `AskStream::final_response()` | Drain the stream and return the final response text. |
| `client.interrupt()` | Abort the running ask; its stream ends with `Error::Interrupted`, and the connection stays usable. |
| `client.list_tools()` / `client.list_agents()` | Introspection. |
| `client.list_sessions()` | Persisted conversations, newest first (`SessionSummary`). |
| `client.get_session(id)` | Load one stored session's messages, oldest first. |
| `client.delete_session(id)` | Delete a stored session's messages from the store. |
| `client.info()` | Model + provider the server was started with (`ServerInfo`). |
| `client.usage(session_id)` | Session context usage vs window (`Usage`). Zeros while an ask streams. |
| `client.ping()` | Liveness check. |
| `client.session_id()` | Session id for this connection's conversation (after an ask); persist it and pass it back via `.session_id(...)` to resume later. |
| `client.close()` | Close the connection, aborting any running ask. |

One `ask` runs per connection at a time; starting a second returns
`Error::Busy` until the first completes or is interrupted.

## Shared conversation model

The `session` module (`Block`, `Phase`, `SessionModel`) holds the transcript,
idle/busy state, and session id. It applies live `AgentEvent`s and replays
stored `Message`s identically, so every front-end renders conversations the
same way and it stays unit-testable without I/O.

```rust
use gewu_client::{Phase, SessionModel};

let mut model = SessionModel::new();
model.submit("list the rust files".into());
model.apply_event(&AgentEvent::ContentDelta { delta: "Sure".into() });
model.finish_ask();
assert_eq!(model.phase, Phase::Idle);
```

The wire protocol is defined by the `gewu-protocol` crate and documented in
[`gewu-server/README.md`](../gewu-server/README.md).
