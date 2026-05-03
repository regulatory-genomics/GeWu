# GeWu (格物)

> "致知在格物，物格而后知至。"  -- 《大学》

GeWu (格物) is a comprehensive framework for building and deploying AI agents. It provides a modular architecture with reusable components for core logic, memory management, provider integrations, and a suite of standard tools.

## Crates

- **`gewu-core`**: The core library providing the foundational abstractions for agents, tools, memory, and LLM providers.
- **`gewu-providers`**: Implementations of LLM providers (e.g., OpenAI, Minimax) for `gewu-core`.
- **`gewu-storage`**: Persistent memory implementation using SQLite via `sqlx`.
- **`gewu-tools`**: A collection of standard tools (file system, web fetch, search, bash) and MCP (Model Context Protocol) support.
- **`gewu-runtime`**: Plugin host, runtime factory, and specialist assembly engine.
- **`gewu-cli`**: A command-line interface for interacting with GeWu agents.

## Getting Started

To use GeWu, add the necessary crates to your project:

```toml
[dependencies]
gewu-core = "0.1.0"
gewu-providers = "0.1.0"
gewu-tools = "0.1.0"
```

### Running the CLI

You can interact with GeWu agents directly using the CLI:

```bash
cargo run --bin gewu-cli -- --help
```

### Configuration

The CLI uses a JSON config file to configure the LLM provider. By default, it looks for `config.json` in the project root, or `~/.gewu/config.json`.

**Config file location priority:**
1. `--llm-config <path>` CLI argument
2. `config.json` in project root (if exists)
3. `~/.gewu/config.json` (if exists)

**Example `config.json`:**
```json
{
  "endpoint": "https://api.openai.com/v1",
  "api_key": "sk-...",
  "model": "gpt-4"
}
```

**Configuration priority (highest to lowest):**
1. CLI arguments (`--endpoint`, `--api-key`, `--model`)
2. Environment variables (`OPENAI_ENDPOINT`, `OPENAI_API_KEY`, `OPENAI_MODEL`)
3. Config file
4. Defaults (none - all values must be provided)

## Plugin System

GeWu supports domain plugins that contribute specialist agents and tools. Plugins are loaded at startup via the `PluginHost`, driven by the `plugins` section in your config file.

GeWu also supports YAML runtime plugins for HTTP-backed tools and lifecycle hooks. Runtime plugins do not require recompiling GeWu: place `*.yaml` manifests in `.gewu/runtime-plugins/`, `~/.gewu/runtime-plugins/`, or directories listed in `runtimePluginDirs`.

```jsonc
{
  "runtimePluginDirs": [
    "/home/gilberthan/disk1/projects/gewu-memory/examples"
  ]
}
```

A runtime plugin can expose tools such as `memory_search` and hook into lifecycle events such as `specialist.system_prompt.transform`, `agent.event`, and `tool.execute.after`. See `examples/gewu-memory-runtime-plugin.yaml` for a memory daemon configuration.

### Managed Tachyon Browser Sidecar

GeWu web can manage `tachyon-browser` as a local HTTP sidecar for genome canvas
data and `genome.*` tools. This keeps Tachyon's genomics stack out of GeWu core
while letting users start a single GeWu web process.

```jsonc
{
  "tachyonBrowser": {
    "enabled": true,
    "autoStart": true,
    "url": "http://127.0.0.1:8400",
    "command": "/home/gilberthan/disk1/projects/tachyon-browser/target/release/tachyon-browser",
    "args": [
      "--host", "127.0.0.1",
      "--port", "8400",
      "--registry", "/path/to/tachyon-browser-registry.json",
      "--genes", "/path/to/gene-info.ucf"
    ],
    "healthPath": "/api/health",
    "startupTimeoutMs": 10000
  }
}
```

Startup behavior:

1. If `url` is already healthy, GeWu reuses it.
2. If not healthy and `autoStart=true`, GeWu spawns `command` with `args`.
3. GeWu waits for `healthPath`, then registers `genome.*` tools.
4. The resolved URL is injected into `index.html` as
   `<meta name="tachyon-browser-url" ...>`, so `tachyon-canvas` can fetch
   `/api/browser/*` from the sidecar.
5. If GeWu started the sidecar, GeWu terminates it on shutdown.

`TACHYON_BROWSER_URL` remains supported as an override for using an already
running external sidecar.

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

Genome analysis tools are provided by `tachyon-browser-client`, a Rust crate inside
the [tachyon-browser](../tachyon-browser) repo. Configure `tachyonBrowser` in your
config to enable them.

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
