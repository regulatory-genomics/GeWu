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
| **`gewu-runtime`** | Plugin host, runtime factory, runtime plugin loading, workflows, and specialist assembly engine. |
| **`gewu-cli`** | Command-line interface for interacting with GeWu agents. Supports multiple agent types (build, explore, general) with configurable tools and permissions. |
| **`gewu-web`** | Web UI built with Axum, providing a browser-based interface with SSE streaming for real-time agent interactions. |
| **`gewu-gateway`** | Gateway services for catalog, inspector, scheduler, execution, and project APIs. |
| **`gewu-graph`** | Stateful graph execution library for building complex workflow orchestration with conditional routing, parallel execution, and nested subgraphs. |
| **`gewu-canvas`** | Canvas state and tooling for browser-based graph and genomics visualization. |
| **`gewu-rag`** | RAG (Retrieval Augmented Generation) system with document processing (PDF, text), embedding generation, and vector storage integration (Qdrant). |
| **`gewu-experience`** | Experience memory system with Qdrant vector embeddings for semantic recall of past problem-solving experiences. Includes experience store and recall tools. |
| **`gewu-evolve`** | Self-evolution capabilities including self-critique, Python sandbox execution, skill library for tool persistence, and LLM-based tool generation. |

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                         gewu-cli                            │
│                         gewu-web                            │
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
| **evolve** | Self-evolving agent with experience memory and skill creation | Off | Build tools + python_execute, skill_create, skill_list, skill_load + experience tools |

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
| Web Fetch | `web_fetch` | Fetch and parse web pages to markdown |
| Web Search | `web_search` | Search the web |
| Code Search | `code_search` | Search code using grep patterns |
| Todo Read | `todo_read` | Read the task list |
| Todo Write | `todo_write` | Write/update the task list |
| Batch | `batch` | Execute multiple tools in parallel |
| Python Execute | `python_execute` | Execute Python code in a sandboxed environment |
| Skill Create | `skill_create` | Create a new skill from natural language description |
| Skill List | `skill_list` | List all available skills in the skill library |
| Skill Load | `skill_load` | Load a skill from the skill library by name |
| Experience Store | `experience_store` | Store a successful task resolution for future recall |
| Experience Recall | `experience_recall` | Query past experiences semantically |

## Getting Started

### Running the CLI

```bash
cargo run --bin gewu-cli -- --help
```

### Running the Web UI

```bash
cargo run --bin gewu-web -- --port 8080
```

### Configuration

The CLI and Web UI use a JSON config file. By default, they look for `config.json` in the project root, or `~/.gewu/config.json`.

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
  "mcp_servers": []
}
```

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

## Plugin System

GeWu supports domain plugins that contribute specialist agents and tools. Plugins are loaded at startup via the `PluginHost`, driven by the `plugins` section in your config file.

### Project-local Skills

GeWu scans Markdown skills from project-local `.gewu/skills/**/SKILL.md` by default. Skill files use YAML frontmatter with at least `name` and `description`.

Specialists opt in with manifest `skills: [...]`. A specialist receives a compact `<available_skills>` list containing only those declared skill names and descriptions; full skill bodies are not injected automatically. When the specialist needs deeper instructions, it calls `load_skill` to load one of its declared skills on demand.

Additional skill directories can be provided via `skillDirs`. `~/.gewu/skills` is not scanned unless it is explicitly listed there.

`skills: [...]` is an access boundary for summaries, `load_skill`, and skill-declared `capabilities`. If a skill declares `capabilities`, GeWu makes those capabilities available only to specialists that list that skill.

Specialists opt in with `skills: - gene-locus-resolver`. The gene workflow lives at `.gewu/skills/gene-locus-resolver/SKILL.md` and instructs agents to call `gene.resolve_symbol` before Genome Canvas open/update calls for gene-symbol requests.

The web chat transcript shows `load_skill` tool calls like any other tool call, but the web UI does not show a global Skills panel.

GeWu also supports YAML runtime plugins for HTTP-backed tools and lifecycle hooks. Runtime plugins do not require recompiling GeWu: place `*.yaml` manifests in `.gewu/runtime-plugins/`, `~/.gewu/runtime-plugins/`, or directories listed in `runtimePluginDirs`.

```jsonc
{
  "runtimePluginDirs": [
    "/home/gilberthan/disk1/projects/gewu-memory/examples"
  ]
}
```

A runtime plugin can expose tools such as `memory_search` and hook into lifecycle events such as `specialist.system_prompt.transform`, `agent.event`, and `tool.execute.after`. See `examples/gewu-memory-runtime-plugin.yaml` for a memory daemon configuration.

### Plugin Suites

GeWu web can load domain integrations as `pluginSuites`. A suite declares its
manifest, sidecars, frontend renderer assets, and runtime tools from one config
entry. This keeps domain stacks such as Tachyon out of `gewu-web` while still
letting users start a single GeWu web process.

```jsonc
{
  "pluginSuites": [
    {
      "id": "tachyon",
      "enabled": true,
      "manifest": "/path/to/tachyon-browser/plugin.yaml",
      "config": {
        "browser": {
          "url": "http://127.0.0.1:8400",
          "autoStart": true,
          "command": "/path/to/tachyon-browser/target/release/tachyon-browser",
          "args": [
            "--host", "127.0.0.1",
            "--port", "8400",
            "--registry", "/path/to/tachyon-browser-registry.json",
            "--genes", "/path/to/gene-info.ucf"
          ],
          "healthPath": "/api/health",
          "startupTimeoutMs": 10000
        },
        "renderer": {
          "kind": "genome",
          "style": "http://127.0.0.1:8400/assets/tachyon-canvas.css",
          "script": "http://127.0.0.1:8400/assets/tachyon-canvas.iife.js"
        }
      }
    }
  ]
}
```

Startup behavior:

1. If the suite sidecar `url` is already healthy, GeWu reuses it.
2. If not healthy and `autoStart=true`, GeWu spawns `command` with `args`.
3. GeWu adds the manifest's directory to runtime plugin loading, registering
   tools such as `genome.*` through the manifest rather than direct Rust deps.
4. GeWu injects `window.__GEWU_PLUGINS__` plus the declared frontend assets
   into `index.html`.
5. The browser can reach sidecars through `/api/plugins/{plugin}/{sidecar}/*`.
   `/api/browser/*` remains as a compatibility proxy for Tachyon.
6. If GeWu started a sidecar, GeWu terminates it on shutdown.

Deprecated `tachyonBrowser` and `tachyonPluginSuite` config blocks are still
accepted and normalized to the Tachyon plugin suite for migration.

### Configuring a Plugin

Add a `plugins` block to your `config.json`:

```json
{
  "endpoint": "https://api.openai.com/v1",
  "api_key": "sk-...",
  "model": "gpt-4",
  "plugins": {
    "genome": {
      "enabled": true,
      "db_root": "/path/to/ucf_db",
      "default_species": "Homo sapiens",
      "default_genome_build": "GRCh38",
      "default_geneset_collection": "msigdb_h_hallmark"
    }
  }
}
```

Each key under `plugins` is a plugin name. The object is passed as `plugin_config` to the plugin constructor. Set `"enabled": false` to disable a plugin without removing its config.

### Building with a Plugin

GeWu core does not ship built-in domain plugins. Plugin integration happens via
composition binaries that depend on `gewu-cli` or `gewu-web` as libraries and register
plugin constructors at startup.

Genome analysis tools are provided by the Tachyon plugin manifest in the
[tachyon-browser](../tachyon-browser) repo. Configure a `pluginSuites` entry with
`id: "tachyon"` to enable them.

### Listing Available Agents

```bash
cargo run --bin gewu-cli -- --list-agents
```

### Writing a Plugin

A plugin implements the `Plugin` trait from `gewu-core`:

```rust
use gewu_core::traits::{Plugin, ToolFactory, PluginPlatformContext};
use gewu_core::types::SpecialistDescriptor;

pub struct MyPlugin { /* ... */ }

impl Plugin for MyPlugin {
    fn name(&self) -> &str { "my-plugin" }
    fn tool_factories(&self) -> Vec<Box<dyn ToolFactory>> { /* ... */ }
    fn specialist_descriptors(&self) -> Vec<SpecialistDescriptor> { /* ... */ }
}

pub fn build_my_plugin(ctx: &PluginPlatformContext) -> anyhow::Result<Box<dyn Plugin>> {
    let config = serde_json::from_value(ctx.plugin_config.clone())?;
    Ok(Box::new(MyPlugin::new(config, ctx.provider.clone(), ctx.pool.clone())))
}
```

Then register it from your own composition binary:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    gewu_cli::run_with_plugins(|registry| {
        registry.register("my-plugin", my_plugin::build_my_plugin);
    })
    .await
}
```

## Security Model

- **Loopback default**: `cargo run --bin gewu-web` binds to `127.0.0.1` with no auth required.
- **Password mode**: `--password <pw>` enables cookie-based auth (argon2 hashed). The login page is served at `/login`.
- **Non-loopback guard**: binding to `0.0.0.0` or a public IP without `--password` refuses to start unless `--allow-insecure-bind` is passed.
- **Cookie auth**: `gewu_session` cookie with `HttpOnly; SameSite=Strict; Path=/`. No URL tokens.
- **Security headers**: CSP, X-Frame-Options: DENY, X-Content-Type-Options: nosniff, Referrer-Policy: no-referrer.

### Config Precedence

For plugin-provided tools, the value resolution order is:

1. Explicit tool arguments (highest priority)
2. Session-level metadata
3. Plugin config defaults (from `config.json`)
4. Environment variable fallback
