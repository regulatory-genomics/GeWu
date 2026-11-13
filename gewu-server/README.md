# gewu-server

`gewu-server` is the command-line interface for the GeWu agent framework. It provides an interactive shell for chatting with an AI agent and executing tools, and it can run as a server so other programs can connect to it.

## Features

- **Interactive Shell**: Chat with an AI agent directly from your terminal.
- **Server Mode**: Expose an agent over WebSocket or TCP for other programs (`serve`).
- **Persistent Memory**: Automatically saves conversation history to a local SQLite database (`~/.gewu/gewu.db`).
- **Tool Execution**: Supports various tools (File System, Bash, Web Fetch, etc.) out-of-the-box.
- **MCP Support**: Loads MCP tools from the unified config file.
- **Customizable LLM**: Supports OpenAI-compatible endpoints via `OPENAI_BASE_URL` and `OPENAI_API_KEY`.

## Usage

### Run an Agent

Start a session with a specific agent type:

```bash
cargo run -p gewu-server -- run --agent build --interactive
```

This will launch an interactive chat session with the `build` agent.

### Provide Initial Prompt

You can provide an initial prompt to start the conversation:

```bash
cargo run -p gewu-server -- run --agent explore "Analyze the project structure"
```

### Run as a Server

Expose an agent so other programs can connect, send prompts, and receive
streamed agent events:

```bash
cargo run -p gewu-server -- serve
```

This starts two listeners (Ctrl+C to stop):

- `ws://127.0.0.1:8420` — WebSocket, one JSON message per text frame
- `tcp://127.0.0.1:8421` — newline-delimited JSON, one message per line

Options: `--host`, `--ws-port`, `--tcp-port`, plus all shared options
(`--agent`, `--model`, `--provider`, `--db-path`, `--llm-config`, ...).

#### Authentication

Pass `--token <secret>` (or `GEWU_SERVE_TOKEN`) to require clients to
authenticate before any other operation:

```jsonc
{"id": "0", "op": "auth", "token": "secret"}   // -> {"type": "auth", "id": "0", "ok": true}
```

A wrong token gets `ok: false` and the server closes the connection. Binding
a non-loopback host without `--token` is refused at startup unless
`--allow-anonymous` is passed explicitly.

#### Line limits

TCP lines longer than 1 MiB (or lines that are not valid UTF-8) are answered
with an `error` frame and skipped; the connection stays usable. Inbound
WebSocket messages are capped at the same size.

#### Wire Protocol

Every message is a JSON object. `id` is a client-chosen correlation ID
echoed on every reply.

**Client → Server:**

```jsonc
{"id": "1", "op": "ask", "prompt": "List the rust files", "session_id": "optional", "agent": "build"}
{"id": "2", "op": "interrupt"}          // abort the running ask on this connection
{"id": "3", "op": "list_tools"}
{"id": "4", "op": "list_agents"}
{"id": "5", "op": "list_sessions"}      // persisted conversations, newest first
{"id": "6", "op": "get_session", "session_id": "s1"}  // one session's messages
{"id": "7", "op": "delete_session", "session_id": "s1"}  // remove it from the store
{"id": "8", "op": "info", "session_id": "s1"} // model, roots and this session's active root
{"id": "9", "op": "switch_workspace", "workspace": "frontend", "session_id": "s1"}
{"id": "10", "op": "usage", "session_id": "s1"}  // context tokens vs limit
{"id": "11", "op": "ping"}
{"id": "0", "op": "auth", "token": "secret"}  // required first on token-protected servers
```

**Server → Client:**

```jsonc
{"type": "ready", "id": "1", "request_id": "c3f1...", "session_id": "s1", "agent": "build"}
{"type": "event", "id": null, "request_id": "c3f1...", "event": {"type": "content_delta", "delta": "Hello"}}
{"type": "event", "id": null, "request_id": "c3f1...", "event": {"type": "tool_start", "tool": "bash", "input": "{...}"}}
{"type": "result", "id": null, "request_id": "c3f1...", "session_id": "s1", "response": "Done."}
{"type": "interrupted", "id": "2", "request_id": "c3f1..."}
{"type": "error", "id": "1", "error": "an ask is already in progress"}
{"type": "pong", "id": "11"}
{"type": "info", "id": "8", "model": "gpt-4o", "provider": "openai", "working_directory": "/home/me/frontend", "active_workspace": "frontend", "workspaces": [{"id": "cwd", "path": "/home/me/project"}, {"id": "frontend", "path": "/home/me/frontend"}]}
{"type": "workspace_switched", "id": "9", "session_id": "s1", "workspace": {"id": "frontend", "path": "/home/me/frontend"}}
{"type": "usage", "id": "10", "used_tokens": 34_000, "context_limit": 128_000, "busy": false}
{"type": "auth", "id": "0", "ok": true}
{"type": "tools", "id": "3", "tools": ["read_file", "bash", "..."]}
{"type": "agents", "id": "4", "agents": ["build", "explore", "general"]}
{"type": "sessions", "id": "5", "sessions": [{"session_id": "s1", "message_count": 12, "updated_at": "2026-11-03 10:42:00"}]}
{"type": "session", "id": "6", "session_id": "s1", "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]}
{"type": "deleted", "id": "7", "session_id": "s1"}
```

`event` payloads are the agent's own events (`content_delta`,
`reasoning_delta`, `tool_start`, `tool_finish`, `usage`, `finish`, `error`) as
serialized JSON. The `usage` event carries the running context estimate
(`used_tokens`/`context_limit`) and is emitted at each turn boundary so
clients can update the usage gauge mid-ask. Streamed `event`/`result` frames
always carry `id: null`
(they come from the background ask task); replies (`ready`, `interrupted`,
`error`, `pong`, `tools`, `agents`, `auth`, `usage`) echo the request's
`id`, and `error` echoes it whenever it can be extracted.

**Sessions:** each connection gets an agent lazily on its first
session-needing request; conversation context persists for the life of the
connection (and in SQLite across reconnects). Pass a `session_id` (e.g. from
a previous `ready`/`result` message or a CLI run) to resume an existing
conversation. The first request's `session_id`/`agent` bind the connection;
a later request naming a *different* session or agent is rejected with a
`session mismatch` / `agent mismatch` error instead of being silently
ignored.

**Concurrency:** one `ask` runs at a time per connection, and a session
busy with an ask on one connection rejects asks from other connections.
While an ask is running, `usage` replies `0/0` with `busy: true` (measuring
would require the agent lock the ask holds); `interrupt` aborts the ask. If
the interrupted ask never produced output, its prompt is dropped from the
history so it cannot steer the next ask; exchanges that already produced
tool/assistant output are kept (with dangling tool calls repaired).

#### Rust client

The `gewu-client` library implements this protocol, with the `gewu-gui`
browser UI (`cargo run -p gewu-gui`) as its front-end:

```rust
use gewu_client::Client;

let client = Client::tcp("127.0.0.1", 8421).await?;
let mut ask = client.ask("What is 2+2?").send().await?;
while let Some(frame) = ask.next_frame().await { /* ... */ }
```

#### Example client (Python, TCP)

```python
import json, socket

s = socket.create_connection(("127.0.0.1", 8421))
f = s.makefile("r")

def send(**msg):
    s.sendall((json.dumps(msg) + "\n").encode())

send(id=1, op="ask", prompt="What is 2+2?")
while True:
    m = json.loads(f.readline())
    if m["type"] == "event" and m["event"]["type"] == "content_delta":
        print(m["event"]["delta"], end="", flush=True)
    elif m["type"] == "result":
        print("\nfinal:", m["response"])
        break
```

### View Persistent Memory

List stored sessions without initializing an LLM provider:

```bash
cargo run -p gewu-server -- view
```

Display one session's messages:

```bash
cargo run -p gewu-server -- view <SESSION_ID>
```

Use `--db-path` with either subcommand to select another database:

```bash
cargo run -p gewu-server -- view <SESSION_ID> --db-path /path/to/gewu.db
```

### Configuration

Configuration is loaded from `config.json` or `config.jsonc` (with comments support). The CLI searches for config in this order:
1. `./config.json` or `./config.jsonc` (project directory)
2. `~/.gewu/config.json` or `~/.gewu/config.jsonc` (home directory)

If both `.json` and `.jsonc` exist in the same location, `.json` takes precedence and a warning is shown.

**Config options:**

| Option | Description |
|--------|-------------|
| `base_url` | Custom provider base URL (e.g., `http://localhost:8080/v1`) |
| `api_key` | API key for the LLM provider |
| `model` | Model to use (default: `gpt-5.3-codex`) |
| `provider` | Provider protocol: `openai` or `anthropic` |
| `log` | Path to a log file where every raw LLM request is recorded as JSONL (append mode) |
| `mcpServers` | MCP server configurations (see below) |

**Environment variables** (take precedence over config file):
- `OPENAI_API_KEY`
- `OPENAI_BASE_URL`
- `OPENAI_MODEL`

**Example `config.json`:**

```jsonc
{
  // LLM settings
  "base_url": "http://localhost:8080/v1",
  "api_key": "sk-...",
  "model": "gpt-4o",
  "provider": "openai",

  // MCP servers
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/dir"]
    }
  }
}
```

### Run Options

- `--llm-config <PATH>`: Load configuration from a specific file (overrides default search path).
- `--list-agents`: List available agent types.
- `--list-models`: List available models from the configured provider.
- `--session-id <ID>`: Resume a specific session.
- `--db-path <PATH>`: Use a custom database file.

Show command-specific options with `gewu-server run --help` or `gewu-server view --help`.
