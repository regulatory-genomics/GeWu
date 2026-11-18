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

## Build and test

`rust-toolchain.toml` pins Rust 1.96.0, required by the GUI math renderer.

```bash
cargo test --workspace
```

This includes client/server transport tests and the run lifecycle regressions
(cancellation, deadlines, tool-call budgets and resumable tool history).

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
# Application plugins

GeWu's **Applications** panel opens and runs configured app adapters alongside chat. See [installation](docs/plugins/installation.md) for package/config steps and the [adapter protocol](docs/plugins/protocol-v1.md) for integration.
