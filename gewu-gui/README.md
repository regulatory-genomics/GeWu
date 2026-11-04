# gewu-gui

Browser-based GUI for a GeWu server (`gewu-server serve`), built on the
[`gewu-client`](../gewu-client/README.md) library.

Start a server first:

```bash
cargo run -p gewu-server -- serve
```

Then launch the GUI and open <http://127.0.0.1:8430>:

```bash
cargo run -p gewu-gui
# or open the browser automatically
cargo run -p gewu-gui -- --open
```

The GUI is a stateful proxy, not a separate agent runtime. It opens its own
upstream connections to `gewu-server` and serves a single-page app to the
browser, so the server does not need to be exposed to every browser.

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `--host` | `127.0.0.1` | Host the GUI's HTTP/WebSocket listener binds to; use `0.0.0.0` for remote browsers |
| `--port` | `8430` | Port for the GUI's HTTP and WebSocket listener |
| `--server` | `ws://127.0.0.1:8420` | Upstream server: `ws://`, `wss://`, or `tcp://host:port` (also `GEWU_SERVER`) |
| `--token` | — | Shared token required from browsers (also `GEWU_GUI_TOKEN`) |
| `--allow-anonymous` | off | Permit a non-loopback bind without a token |
| `--open` | off | Open the GUI in the default browser after starting |
| `--log-level` | `info` | `error`, `warn`, `info`, `debug`, or `trace` |

A bare `host:port` in `--server` is treated as TCP, matching `gewu-tui`'s
default.

## Remote access

The browser only needs to reach the GUI port. The upstream server connection
is made by `gewu-gui`.

```bash
# on the machine running the agent
cargo run -p gewu-server -- serve

# expose the GUI to the network, protected by a token
cargo run -p gewu-gui -- --host 0.0.0.0 --token "$(openssl rand -hex 16)"
```

The first browser frame must then carry the token; the page asks for it and
remembers it in `localStorage`. Wrong tokens are rejected with `unauthorized`
and the socket is closed. Supplying a token on a loopback bind is allowed but
optional.

**`gewu-gui` runs an agent with tool access.** A browser that passes the token
— or any browser at all when `--allow-anonymous` is used off-loopback — can
run shell commands on the GUI's machine. Bind to loopback unless you have a
reverse proxy, TLS, and a strong token.

## Tabs

On startup the GUI opens **every stored session as a tab** — oldest on the
left, newest on the right — and focuses the newest. Titles come from the first
user prompt, falling back to the short session id or `new`. Each tab has its
own upstream connection, so an ask can keep streaming in a background tab while
you work in another. `+` opens a fresh session; `×` closes the tab **and
permanently deletes that session from the server's store**, with no
confirmation.

## Interaction

| Control | Action |
|---------|--------|
| `Enter` | Send the prompt |
| `Shift+Enter` | Insert a newline |
| `Esc` | Interrupt the running ask |
| Send / Stop | Send the composer / interrupt (buttons) |
| Tab click | Switch sessions |
| `×` on a tab | Close the tab and delete the session |
| Tool row / `… +N lines` | Expand or collapse tool output |
| `+ Explored: …` | Expand or collapse a run of tool calls |

The transcript streams assistant text and replaces it with rendered Markdown
when the reply finishes. The status line shows the phase, `provider/model`, the
short session id, and context usage (`34k (26%)`), turning yellow from 80%.
Scrolling up pins the history; scrolling back to the bottom re-follows streamed
output. A dropped GUI connection reconnects with backoff and asks for a fresh
snapshot; an `unauthorized` error returns to the token prompt.

## Rendering and safety

Server-rendered HTML fragments are the only thing inserted with `innerHTML`.
All user- and model-authored text is HTML-escaped by Rust, Markdown raw HTML is
neutralized into text, and `javascript:`, `vbscript:`, and `data:` link/image
destinations are replaced with `#`. Streamed deltas are appended with
`textContent`, so partial assistant output cannot inject markup either.

## Layout

```
gewu-gui
  src/cli.rs        flags and upstream endpoint parsing
  src/protocol.rs   browser <-> GUI JSON messages
  src/workspace.rs  tabs over the shared SessionModel
  src/session.rs    per-browser workspace, upstream connections, event loop
  src/view.rs       transcript/Markdown/tool/status rendering to HTML
  src/server.rs     axum router, static assets, token auth, /ws
  src/lib.rs        startup, security checks, listener
  assets/           index.html, app.js, app.css (embedded at build time)
  assets/fonts/     Inter and JetBrains Mono (woff2, SIL OFL 1.1; see OFL-*.txt)
```

Conversation state lives in `gewu-client::session` and is shared with
`gewu-tui`, so both front-ends replay history and apply streamed events
identically. Rendering stays in Rust; the browser script only swaps in HTML
snapshots and appends text deltas.
