# Session directory sidebar

Status: done

## Goal

Add a left sidebar that groups existing session tabs by their selected server directory, lets users switch sessions, and creates a new session directly inside a chosen directory.

## Verified current behavior

- `gewu-gui` already restores every top-level persisted session into `Workspace::tabs`.
- Each `TabInfo` already carries the session title, busy state, session ID, and workspace ID.
- The current right sidebar is the file/tools sidebar; the transcript grid has one main column plus that right sidebar.
- New sessions can already be created with a preselected workspace through `ClientMsg::CreateTab { workspace, path }`.

## Design

- Add a left `session-sidebar` to the existing shell. Render workspace groups from the current tab list plus the workspace catalog; each group contains session rows and a `+` button.
- Clicking a session row sends the existing `switch_tab` operation. Closing a row sends the existing `close_tab` operation.
- Clicking a workspace group's `+` sends `create_tab` directly with that workspace, bypassing the global chooser. The first ask still creates the upstream session, so empty tabs remain sessionless.
- Add the rendered session-sidebar HTML to `Snapshot` and emit a targeted `SessionSidebar` update whenever tab state changes. Keep the existing top tab bar as a fast-switching view.
- Expand the desktop grid to `session sidebar / transcript / file sidebar`; hide the session sidebar on narrow screens to preserve the current mobile layout.

## Non-goals

- No new persistence table or session ownership model.
- No session search or date-based history page in this iteration.
- No removal of the existing top tabs or right file/tools sidebar.

## Acceptance gates

- [x] Desktop GUI shows workspace groups on the left and sessions nested below them.
- [x] Session rows switch the active transcript and retain their workspace.
- [x] Workspace `+` creates a new blank tab preselected to that workspace.
- [x] Closing a session from the left sidebar uses the existing close/delete behavior.
- [x] Sidebar updates on startup, tab creation, switching, busy/idle changes, and closing.
- [x] Existing top tabs and right file sidebar continue to work.
- [x] Protocol/view tests cover sidebar rendering and snapshot/update messages.
- [x] `cargo test --workspace --locked` and `cargo build -p gewu --release --locked` pass.

## Tasks

1. Add session-sidebar rendering and protocol messages/tests.
2. Add three-column layout and browser event wiring for switching/creating/closing sessions.
3. Run full verification, commit, push GitHub/Gitea, and restart the deployed service.
