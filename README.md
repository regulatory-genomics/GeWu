# GeWu (格物)

> "致知在格物，物格而后知至。" -- 《大学》

GeWu (格物) is a coding-agent app written in Rust. One command starts an
agent server plus a browser chat UI: the agent plans, writes and runs code
with tools (bash, file editing, code search, web fetch/search), delegates to
subagents, and runs background workflows — streaming responses as it works
and keeping sessions in SQLite across restarts. It works with the OpenAI and
Anthropic APIs as well as OpenAI-compatible services (DeepSeek, GLM,
Moonshot, proxies, and local models).

## Install

Prebuilt binaries are published for Linux x86_64 (glibc >= 2.28) and macOS
arm64. The installer downloads the latest release, verifies its checksum, and
puts the binary in `~/.local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/regulatory-genomics/GeWu/main/install.sh | bash
```

Set `GEWU_INSTALL_DIR` to install elsewhere, or `GEWU_VERSION=v0.1.3` to pin a
release. Add the install directory to your `PATH` if the script asks you to.

On other platforms, build from source with a Rust toolchain (see
[rustup.rs](https://rustup.rs)). From the repository root:

```bash
cargo install --path gewu    # builds and installs the `gewu` binary
```

## Update

At startup, `gewu` checks GitHub for a newer release and prints a notice in
the terminal and the session sidebar. It never installs on its own. To update
explicitly:

```bash
gewu update          # download, verify, and replace the current binary
gewu update --check  # only report what is available
```

`gewu update` verifies the release archive against the published SHA-256
checksum, and the new version takes effect on the next launch. Set
`GEWU_NO_UPDATE_CHECK=1` or pass `--no-update-check` to disable the startup
check.

## Configure

GeWu needs one LLM endpoint. Either export credentials:

```bash
export OPENAI_API_KEY=sk-...     # or ANTHROPIC_API_KEY, DEEPSEEK_API_KEY,
                                 # GLM_API_KEY, MOONSHOT_API_KEY
export OPENAI_MODEL=gpt-4o
export GEWU_PROVIDER=openai      # openai | anthropic | openai-compatible |
                                 # deepseek | glm | moonshot
```

Or write a `config.jsonc` (searched in the working directory, then
`~/.gewu/`; pass a custom path with `--llm-config`):

```jsonc
{
  "endpoints": {
    "openai": {
      "base_url": "https://api.openai.com/v1",
      "api_key": "sk-...",
      "models": {
        "gpt": { "provider": "openai", "reasoning_effort": "high" }
      }
    }
  }
}
```

## Use

```bash
gewu                    # agent server + web GUI → open the printed URL
                        # (default http://127.0.0.1:8430)
gewu --no-gui           # agent server only
gewu serve --host 0.0.0.0 --token "$(openssl rand -hex 16)"
                        # headless server for remote clients; a token is
                        # required off loopback
```

### Terminal

The same binary supports interactive chat and one-shot jobs:

```bash
gewu cli                       # new chat without a workspace
gewu cli "Hello"                # send the first message and keep chatting
gewu cli --resume              # pick a saved session
gewu cli --resume SESSION_ID
gewu cli --last                # latest saved session for this workspace
gewu exec "Explain this repo"  # print the final answer and exit
git diff | gewu exec "Summarize this diff"
gewu exec --json "List the test suites" > events.jsonl
```

Both commands start a private loopback server by default. It uses the same
configuration and database as the web app and shuts down when the command
exits. Put startup options before the subcommand, for example
`gewu --llm-config config.jsonc --db-path project.db cli`.

To share live work with the web app, connect to its printed **agent server**
WebSocket URL. For a separate `gewu serve` with default ports, use
`gewu cli --server ws://127.0.0.1:8420 --resume`. Both terminal commands
accept `--server` and read `GEWU_SERVE_TOKEN` for authentication. Project paths
refer to directories on that server. Disconnecting a terminal from an external
server leaves its work running. To stop the current turn, press Ctrl+C before
exiting. A session already loaded by a server can be resumed on that instance.

New chats start without a workspace. Choose a folder with `/cd` in the CLI or **Open workspace** in the web app when you need files, shell commands, or workflow execution. The current conversation stays open.

In chat, Enter sends a message, Alt+Enter or Ctrl+J inserts a newline, and
Ctrl+C interrupts a running turn. While the agent works, Enter sends additional
instructions. Click a tool row to expand it. Use PageUp/PageDown to scroll.
Agent questions accept numbered choices or a written answer when offered.
Type `/` to search commands by name or description. Use ↑/↓ to select,
Tab to complete, Enter to open, and Esc to dismiss. The first seven suggestions
are also available while drafting: Ctrl+P opens the palette without losing
the draft. Closing an inspection panel restores it. The top suggestions
are shortcuts; keep scrolling or typing to discover the rest.

| Area | Commands |
| --- | --- |
| Session | `/new`, `/resume` (alias `/sessions`), `/status`, `/delete`, `/help`, `/exit` |
| Session settings | `/settings` shows effective settings, model defaults, and server lifetime |
| Model and mode | `/model`, `/effort`, `/agent`, `/plan`, `/build`, `/tools` |
| Accounting | `/context` (estimated active window), `/usage` (reported cumulative tokens) |
| Workspace | `/pwd`, `/cd` (alias `/workspace`), `/files`, `/attach` |
| Transcript | `/copy` (macOS), `/export [new-local-path]`, `/revert [turn]`, `/undo` |
| Workflow design | `/workflow`, `/workflow import` (from the last assistant reply) |
| Inspection | `/tasks`, `/subagents`, `/todos`, `/skills`, `/mcp` |

On builds with the application plugin host, open `/plugins` (also in
`/settings`) to install a local package directory, list installed versions,
check a package, or change its installation default. Reopen the local CLI
after installing or toggling a plugin, then select a workspace with `/cd`
to use its tools. Packages with browser views use the web GUI for Preview.
For a signed release, run `gewu plugin search NAME --refresh` and
`gewu plugin marketplace-install ID` on the machine running the host.
When connected with `--server`, installation is managed on that server.

Open `/workflow` to edit the session's workflow JSON and preview its edges.
Ctrl+E loads a BUSCO example into an empty graph. Ctrl+N appends a step,
Ctrl+V checks the graph, Ctrl+S saves the shared draft, and Ctrl+R runs
the saved design after confirmation. For a natural-language design, ask
in `/plan` for a GeWu workflow in a JSON code block, then use
`/workflow import` to review it. Switch to `/build` before running.
Run progress appears in `/tasks`.

For simultaneous web and terminal use on one Mac, start the web launcher
with `gewu --ws-port 8431`, then use
`gewu cli --server ws://127.0.0.1:8431 --resume` from a second terminal.
Choose the same saved session in both clients. The browser's Workflow button
opens the shared draft. Both frontends accept `/build`, `/plan`, `/workflow`, and `/settings` directly. Each session stores one draft in SQLite. Clean
editors follow saves, and a stale editor retains its edits until reloaded.

Model and effort changes apply to the active session. File browsing uses the
server workspace; `/attach PATH` uploads a local file and queues it for the
next message. For larger files, select the file from the server workspace
using `/files`; it is referenced by path rather than uploaded over WebSocket.
Browsing and inspection do not call the model. Workflow lists
include recent runs tracked by the current server, with node details and
confirmed cancellation. Select a node to inspect its reported child sessions.
`/subagents` previews each child’s latest stored instruction and reply.
Use `/settings` to change the model, reasoning effort, mode, or workspace for the active session.
In the local CLI, Settings has separate Model and API key entries. Model lists configured models.
Add custom model asks for an endpoint name, API base URL, model ID and protocol. Reopen GeWu to load the new catalog.
Model selection returns to Settings. API key shows the selected model and URL. Leave the field blank to verify a saved key, or paste a replacement to check model access before saving.
Use F5 for a short inference test on gateways without model discovery. A saved key takes effect for the current local server and is shared by models on that endpoint.
Reverting changes conversation history only. Delete, revert and workflow
cancellation require an explicit selection followed by confirmation.
Legacy sessions without an absolute workspace prompt for a server directory
before continuing.

In exec mode, stdout contains the final answer, and stderr carries diagnostics.
With `--json`, stdout is JSON Lines: a `started` record, `event` records
containing protocol events, then a terminal `result` or `error`. Each record
includes `session_id` (null if startup failed). Exit codes are 0 for success,
1 for execution errors, 2 for command-line syntax errors, 3 when the agent
asks for input, and 130 for an interrupted turn. A question interrupts the
one-shot task; its record identifies the session and question.

### Workspaces

Sessions can start in any of several server-side directories. `cwd` is
always available; add named roots in `config.jsonc`:

```jsonc
{
  "workspaces": {
    "frontend": "../frontend",
    "wiki": "/srv/wiki"
  }
}
```

Relative paths resolve against the startup directory. In the web UI, pick a
root in the FILES panel (`ROOT`), use `＋` to browse and add any directory on
the server host at runtime, or choose a workdir when creating a session from
the left sidebar. Each session remembers its root across restarts; switching
is rejected while a turn is running.

Agent state is stored in `~/.gewu/gewu.db` (override with `--db-path`).
