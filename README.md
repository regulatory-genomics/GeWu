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

Plugins are gated behind Cargo feature flags. For example, to enable the genome plugin:

```bash
cargo build --features genome
cargo run --features genome --bin gewu-cli -- --agent genome-orchestrator
```

### Listing Available Agents

```bash
cargo run --features genome --bin gewu-cli -- --list-agents
```

This shows both builtin agents (build, explore, general) and any plugin-provided specialists.

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

/// Constructor for PluginRegistry
pub fn build_my_plugin(ctx: &PluginPlatformContext) -> anyhow::Result<Box<dyn Plugin>> {
    let config = serde_json::from_value(ctx.plugin_config.clone())?;
    Ok(Box::new(MyPlugin::new(config, ctx.provider.clone(), ctx.pool.clone())))
}
```

Then register it in `gewu-cli/src/main.rs`:

```rust
#[cfg(feature = "my_plugin")]
plugin_registry.register("my-plugin", my_plugin::build_my_plugin);
```

### Config Precedence

For plugin-provided tools, the value resolution order is:

1. Explicit tool arguments (highest priority)
2. Session-level metadata
3. Plugin config defaults (from `config.json`)
4. Environment variable fallback