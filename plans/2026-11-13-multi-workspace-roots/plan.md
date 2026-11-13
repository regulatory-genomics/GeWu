# Multi-workspace roots

Status: done

## Goal

Allow one GeWu server to expose several explicitly configured working-directory roots. Each session selects one active root; all filesystem tools, bash, subagents, workflow agents, server file APIs, and the GUI file tree use that selected root. A config without extra roots behaves exactly like the current single-cwd setup.

## Design

- Config key: `workspaces`, an optional object mapping stable root IDs to paths. The startup cwd is always included as `cwd`; config entries add named roots. Relative paths resolve against the startup cwd.
- Runtime representation: a validated ordered `BTreeMap<id, canonical path>` shared by bootstrap, server, and GUI.
- Session state: persist the selected root ID in SQLite (`session_workspaces`). Legacy sessions default to `cwd`. Root changes are rejected while a turn is running and update the existing agent's active root without rebuilding its conversation.
- Tool boundary: add an optional active working directory to `ToolContext`. Path-sensitive tools prefer that context root and retain their constructor root as a compatibility fallback for direct callers/tests. `PathEnforcement` is applied against the selected root only.
- Delegation: `TaskTool` and workflows pass the parent context's selected root to spawned agents through additive `AgentFactory` methods, preserving existing third-party factory implementations.
- Protocol: add `switch_workspace`; extend `Info` with the catalog and active root. This avoids a second catalog request and remains backward-compatible for clients that ignore unknown info fields.
- GUI: expose a compact workspace selector near the file tree; switching it sends `switch_workspace`, clears the tree cache, and reloads the selected root. File paths remain root-relative in the UI, so the model never receives arbitrary host paths from the selector.

## Non-goals

- No union filesystem or cross-root relative paths.
- No automatic discovery of parent/sibling directories.
- No change to `PathEnforcement::None/Contain/Strict` semantics inside a selected root.
- No migration of existing config files beyond accepting the new optional key.

## Acceptance gates

- [x] Existing config with no `workspaces` still exposes only `cwd` and existing single-root tests pass.
- [x] Configured relative and absolute roots validate, resolve, and reject missing/non-directory roots at startup.
- [x] `contain` and `strict` prevent escape from the selected root while allowing the same relative path in another declared root.
- [x] A session can switch roots while idle; the next tool call, bash command, task subagent, and workflow node use the new root.
- [x] Switching while busy is rejected without changing the running session.
- [x] Root selection survives reconnect and process restart through SQLite session metadata.
- [x] GUI shows all roots and renders the selected root's tree.
- [x] `cargo test --workspace --locked` and `cargo build -p gewu --release` pass.

## Tasks

1. Add root config/catalog resolution and persistence primitives (`gewu-server/src/config.rs`, `bootstrap.rs`, `gewu-storage/src/lib.rs`) with tests.
2. Add dynamic active-root context and update path-sensitive tools (`gewu-core/src/types.rs`, `agent/mod.rs`, `gewu-tools/src/{fs,apply_patch,bash,grep,list}.rs`) with tests.
3. Thread root selection through factories, sessions, server protocol, and server handlers (`task.rs`, `workflow/{engine,manager}.rs`, `registry.rs`, `server/mod.rs`, `protocol/src/lib.rs`).
4. Add GUI root listing/selection and tree cache handling (`gewu-gui/src/{protocol,session,view}.rs`, assets if required) with protocol/view tests.
5. Run workspace verification, review the diff, and rebuild the running binary only after all acceptance gates pass.
