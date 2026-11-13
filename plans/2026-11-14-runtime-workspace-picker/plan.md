# Runtime workspace picker

Status: done

## Goal

Let an authenticated GeWu GUI session add and select server-side directories at runtime, without requiring every directory to be hard-coded in `config.json`.

## Design

- Keep configured `workspaces` as optional initial roots; retain implicit `cwd`.
- Add a server-side directory browser request that lists direct child directories of a requested path and returns a canonical path plus display name. The browser is bound to the server host, so remote GUI sessions choose directories on the machine running GeWu.
- Add a session-scoped `add_workspace` operation. It validates an existing directory, adds it to the server's runtime catalog, and persists the selected root ID/path for the session. Runtime roots are not exposed to the model as a tool.
- Reuse the existing `switch_workspace` operation and active `ToolContext` root propagation.
- GUI adds an `ADD DIRECTORY` action next to the ROOT selector, a small path/browse view, and refreshes the root catalog/tree after adding a directory.
- Keep the existing config-root path as a stable option for deployments that want a fixed allowlist. Runtime selection is an explicit authenticated GUI action, not arbitrary model-controlled filesystem access.

## Non-goals

- No browser-local File System Access API; it would select files on the user's browser machine rather than the GeWu server.
- No model-facing `list_directories` or `add_workspace` tool.
- No automatic discovery of parent directories or recursive indexing.

## Acceptance gates

- [x] A GUI can browse direct server-side directories and add one without editing config.
- [x] The added directory appears in the ROOT selector and can be selected for the current session.
- [x] The selected root persists across GUI reconnect and server restart through session metadata.
- [x] Existing configured roots and single-directory setups continue to work.
- [x] Directory browsing rejects files, missing paths, and invalid traversal inputs safely.
- [x] `cargo test --workspace --locked` and `cargo build -p gewu --release --locked` pass.

## Tasks

1. Add protocol/client/server runtime directory browse/add operations with tests.
2. Add GUI add-directory flow and root-catalog refresh with view/protocol tests.
3. Verify end-to-end behavior, rebuild, restart 8889, and update the task record.
