# GeWu (格物)

> "致知在格物，物格而后知至。" -- 《大学》

GeWu (格物) is a comprehensive framework for building and deploying AI agents. It provides a modular architecture with reusable components for core logic, memory management, provider integrations, workflow orchestration, RAG capabilities, and a suite of standard tools.

## Crates

| Crate | Description |
|-------|-------------|
| **`gewu-core`** | Core library providing the foundational abstractions for agents, tools, memory, and LLM providers. Contains the main Agent runtime with loop-based execution, tool calling, doom loop detection, and history compaction. |
| **`gewu-providers`** | LLM provider implementations supporting OpenAI-compatible APIs and Anthropic Claude. Features streaming responses, tool/function calling, and multi-modal content handling. |
| **`gewu-storage`** | Persistent memory implementation using SQLite via `sqlx`. Enables multi-session support where conversations persist across process restarts. |
| **`gewu-tools`** | Collection of built-in tools for filesystem operations, bash execution, web fetching, code search, and MCP (Model Context Protocol) integration. |
| **`gewu-server`** | Command-line interface for interacting with GeWu agents. Supports build, explore, and general agent profiles with configurable tools and permissions. Also runs as a server (`gewu-server serve`) exposing agents over WebSocket and TCP so other programs can send prompts and receive streamed agent events. |
| **`gewu-protocol`** | Wire protocol shared by the server and its clients: request parsing/serialization (`ClientRequest`) and server reply types (`ServerMessage`). |
| **`gewu-client`** | Async Rust client library for the GeWu server: streaming asks (`Client::ask`), `interrupt`, session listing/loading, tool/agent introspection, and the UI-agnostic conversation model (`SessionModel`) shared by the TUI and GUI. |
| **`gewu-tui`** | Full-screen terminal chat UI built on `gewu-client`: streaming transcript, `$ <command>` tool-call display, scrollback, and session resume. |
| **`gewu-gui`** | Desktop client for the GeWu server, built with egui/eframe. Streaming markdown transcript, collapsible tool cards, interrupt, and a sidebar for resuming persisted sessions. Shares its conversation model and `Client` library with the TUI. |
| **`gewu-graph`** | Stateful graph execution library for building complex workflow orchestration with conditional routing, parallel execution, and nested subgraphs. |
| **`gewu-rag`** | RAG (Retrieval Augmented Generation) system with document processing (PDF, text), embedding generation, and vector storage integration (Qdrant). |
| **`gewu-experience`** | Experience memory system with Qdrant vector embeddings for semantic recall of past problem-solving experiences. Includes experience store and recall tools. |
| **`gewu-evolve`** | Self-evolution capabilities including self-critique, Python sandbox execution, skill library for tool persistence, and LLM-based tool generation. |

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                         gewu-server                           │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                        gewu-core                            │
│  ┌─────────┐  ┌──────────┐  ┌─────────┐  ┌─────────────┐    │
│  │ Agent   │  │  Tool    │  │  LLM    │  │   Memory    │    │
│  │ Runtime │  │  Trait   │  │Provider │  │   Trait     │    │
│  └─────────┘  └──────────┘  └─────────┘  └─────────────┘    │
└─────────────────────────────────────────────────────────────┘
         │              │            │              │
         ▼              ▼            ▼              ▼
┌────────────┐  ┌──────────┐  ┌────────────┐  ┌────────────┐
│gewu-tools  │  │   MCP    │  │   OpenAI   │  │   SQLite   │
│            │  │ Protocol │  │ Anthropic  │  │  Storage   │
│ - bash     │  │          │  │            │  │            │
│ - fs ops   │  └──────────┘  └────────────┘  └────────────┘
│ - grep     │
│ - search   │
│ - batch    │
└────────────┘

┌────────────┐  ┌────────────┐  ┌─────────────────┐  ┌────────────────┐
│ gewu-graph │  │  gewu-rag  │  │gewu-experience  │  │  gewu-evolve  │
│            │  │            │  │                 │  │               │
│ StateGraph │  │  Document  │  │  Experience    │  │ Self-critique │
│ Workflows  │  │  Processing│  │  Store with    │  │ Python sandbox│
│            │  │  Embedding │  │  Qdrant        │  │ Skill library │
│            │  │  Retrieval │  │  embeddings    │  │ Tool builder  │
└────────────┘  └────────────┘  └─────────────────┘  └────────────────┘
```

## Agent Types

The CLI provides pre-configured agent types optimized for different use cases:

| Agent | Description | Safe Mode | Tools |
|-------|-------------|-----------|-------|
| **build** | Default agent for software engineering tasks | Off | All tools including bash, file edit, task management |
| **explore** | Fast agent for exploring codebases | On | Read-only tools, restricted bash |
| **general** | General-purpose agent for research and multi-step tasks | Off | All tools including task management |

## Built-in Tools

| Tool | Name | Description |
|------|------|-------------|
| Bash | `bash` | Execute shell commands with timeout and safe mode |
| Read File | `read_file` | Read files, list directories, handle images/PDFs as base64 |
| Write File | `write_file` | Create or overwrite files |
| Edit File | `edit_file` | Edit files by replacing text with smart matching |
| Glob | `glob` | Find files by glob patterns respecting .gitignore |
| Grep | `grep` | Search file contents with regex support |
| List | `list` | List directory contents |
| Web Fetch | `webfetch` | Fetch and parse web pages to markdown |
| Web Search | `websearch` | Search the web |
| Code Search | `codesearch` | Search code using semantic search |
| Todo Read | `todoread` | Read the task list |
| Todo Write | `todowrite` | Write/update the task list |
| Batch | `batch` | Execute multiple tools in parallel |
| Task | `task` | Launch sub-agents (build/explore/general) for complex tasks; resumable via `task_id` |
| Skill | `skill` | Load skill instructions from config-declared skill directories into the conversation |

## Getting Started

### Running the CLI

```bash
cargo run -p gewu-server -- run --help
```

Run an agent or inspect persisted memory:

```bash
cargo run -p gewu-server -- run "Hello"
cargo run -p gewu-server -- view
cargo run -p gewu-server -- view <SESSION_ID>
```

Run an agent as a server other programs can connect to (WebSocket on port
8420, newline-delimited JSON TCP on port 8421 — see `gewu-server/README.md`
for the wire protocol):

```bash
cargo run -p gewu-server -- serve
```

Connect from the terminal with the `gewu-tui` TUI:

```bash
cargo run -p gewu-tui                    # full-screen chat TUI
cargo run -p gewu-tui -- --session-id <ID>   # resume a conversation
```

Or use the `gewu-gui` desktop window (session history sidebar, markdown,
collapsible tool cards):

```bash
cargo run -p gewu-gui
cargo run -p gewu-gui -- --session-id <ID>
```

Or embed it as a library from Rust:

```rust
use gewu_client::Client;

let client = Client::tcp("127.0.0.1", 8421).await?;
let response = client.ask("Hello").send().await?.final_response().await?;
```

(See `gewu-client/README.md` for the library and `gewu-tui/README.md` for the
terminal UI.)

### Configuration

The CLI uses a JSON config file. By default, it looks for `config.json` in the project root, or `~/.gewu/config.json`.

**Config file location priority:**
1. `--config <path>` CLI argument
2. `config.json` in project root (if exists)
3. `~/.gewu/config.json` (if exists)

**Example `config.json`:**
```json
{
  "endpoint": "https://api.openai.com/v1",
  "api_key": "sk-...",
  "model": "gpt-4",
  "log": "gewu-requests.jsonl",
  "skills": ["skills"],
  "mcp_servers": []
}
```

When `log` is set, every request sent to the LLM provider (including tool
definitions) is recorded as one JSONL line in the file:
`{"timestamp": "...", "request": {...}}`.

When `skills` is set, each directory is scanned for `SKILL.md` files
(`**/SKILL.md`) with `name` (required) and `description` frontmatter. Discovered
skills are listed in the agent's system prompt and can be loaded with the
`skill` tool, which returns the skill's markdown body plus up to 10 sampled
companion files from its directory.

**Configuration priority (highest to lowest):**
1. CLI arguments (`--endpoint`, `--api-key`, `--model`, `--config`)
2. Environment variables (`OPENAI_ENDPOINT`, `OPENAI_API_KEY`, `OPENAI_MODEL`)
3. Config file
4. Defaults (none - all values must be provided)

## Key Features

### Agent Runtime
- **Loop-based execution**: Agent repeatedly calls LLM and executes tools until a final response is generated
- **Doom loop detection**: Prevents infinite loops by detecting repeated identical tool calls
- **History compaction**: Automatically summarizes conversation history when approaching context limits
- **Panic recovery**: Tool executions are sandboxed to prevent crashes from propagating

### LLM Providers
- **OpenAI**: GPT-4, GPT-4-turbo, GPT-3.5-turbo with automatic capability detection
- **Anthropic**: Claude with extended thinking support
- **Multi-modal**: Support for images, audio, video, and PDF inputs where model capabilities allow
- **Streaming**: Server-Sent Events (SSE) for real-time response streaming

### Memory
- **In-memory**: Default volatile memory for single-session use
- **SQLite persistence**: Optional persistent storage for multi-session support
- **Message types**: System, User, Assistant, and Tool messages with media support

### Graph Workflows
- **State management**: Shared state flows through graph nodes with partial updates
- **Conditional routing**: Edges control flow based on node results
- **Parallel execution**: Send state to multiple nodes simultaneously
- **Streaming**: Real-time state updates during execution
