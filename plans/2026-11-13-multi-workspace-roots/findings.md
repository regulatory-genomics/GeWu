# Findings

- Current config has `sandbox` but no workspace-root field. Bootstrap captures `env::current_dir()` once.
- Filesystem tools store `working_directory` in each tool instance and resolve paths against it; `ToolContext` currently carries enforcement but not a root.
- Server sessions create agents with the bootstrap root and server `list_files/read_file/write_file` also hard-code that root.
- GUI receives one `Info.working_directory`, caches paths without a root namespace, and has no workspace switch command.
- Existing SQLite metadata tables for session agent/model are lazily created, so a `session_workspaces` table can preserve legacy DB compatibility.
- The agent factory trait is public and used by tests/workflow code; additive `create_in`/`create_with_in` methods avoid breaking existing implementations.
- `ToolContext` now has an optional selected root and path-sensitive tools retain constructor roots only as compatibility fallbacks; this avoids rebuilding tool registries when a session switches roots.
- The public workflow APIs keep their old signatures; workspace-aware variants are additive (`start_with_inbox_at`, `engine::run_in`).
- Direct protocol file operations with a `session_id` now lazily bind that session before resolving the root, so clients do not need a preceding `usage` request.
- Full verification showed the GUI must restore workspace state before its first Snapshot; emitting a second startup UI sequence broke existing message-order contracts. The final implementation refreshes the cache silently, then emits one consistent initial Snapshot.
