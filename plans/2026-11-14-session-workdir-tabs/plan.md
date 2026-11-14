# Session creation workdir and tab management

Status: done

## Goal

Let the GUI choose the server-side workdir before creating a new session, while keeping each session visible and switchable in a tab.

## Verified current behavior

- `gewu-gui` already maintains one tab per conversation/session in `gewu-gui/src/workspace.rs`.
- `ClientMsg::OpenTab` currently creates an empty local tab immediately; its first ask creates the upstream session with the default `cwd`.
- Existing workspace browsing and `switch_workspace` operate after a session exists.
- The upstream protocol supports session IDs, workspace switching, and persisted `session_workspaces`, but no creation-time workspace field.

## Design

- Add an optional creation-time workspace selector to the GUI protocol. Opening a new tab requests the existing server-side directory browser rather than creating a session immediately.
- Render a compact modal that lists the configured/runtime workspace catalog and lets the user browse/add a server directory. Selecting a workspace creates a new tab with that workspace preselected.
- Carry the selected workspace into the first upstream session request. The server validates it using the same workspace resolution path as `switch_workspace`, persists it for the newly created session, and initializes the primary agent with that root before any ask runs.
- Keep existing tabs as the session manager: each tab retains its session ID, title, busy state, and active workspace; switching tabs restores the corresponding transcript/file tree and upstream observer.
- Keep `OpenTab` compatibility by treating an omitted workspace as `cwd`; do not change existing reconnect or configured-root behavior.

## Non-goals

- No project database or global cross-browser session ownership model.
- No filesystem access from the browser; directory selection remains server-side.
- No redesign of the transcript or existing tab persistence.

## Acceptance gates

- [x] Clicking new session opens a workdir chooser before a session is created.
- [x] Choosing `tachyon`, `wiki`, or a browsed runtime directory creates a new tab whose first `pwd` uses that root.
- [x] Existing tabs can be switched, closed, and resumed with their own session/workspace state.
- [x] Canceling the chooser creates no session and leaves the active tab unchanged.
- [x] Legacy `OpenTab`/missing-workspace requests still default to `cwd`.
- [x] Protocol, server, workspace-state, and GUI rendering tests cover the new flow.
- [x] `cargo test --workspace --locked` and `cargo build -p gewu --release --locked` pass.

## Tasks

1. Add creation-time workspace protocol/client/server support and regression tests.
2. Add GUI new-session workdir chooser, pending-tab state, and tab-management rendering/events.
3. Run full verification, commit, push GitHub/Gitea, and restart the deployed service if the build is used by 8889.
