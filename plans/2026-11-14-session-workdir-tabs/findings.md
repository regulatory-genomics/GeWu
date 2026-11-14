# Findings

- `Workspace::open_blank` creates only local tab state; it does not create an upstream session until the first request.
- `GuiSession::new` restores stored sessions into tabs and `handle_client` already supports switch/close/open tab operations.
- The current `OpenTab` path creates a blank tab directly, so it cannot select a root before the first ask.
- The existing server-side browse/add workspace flow can be reused for the creation chooser, but creation must pass the selected workspace into the first upstream request.
- `ClientRequest::workspace` is an optional creation hint; the server ignores it when a session already has persisted workspace metadata, preventing a reconnect from overwriting the saved root.
- New tabs remain local and sessionless until the first ask. This lets canceling the chooser avoid creating a SQLite session.
- Runtime workspace choices are represented as `runtime:<canonical-path>` and are validated again by the upstream server on the first ask.
- Existing top tabs already provide the session manager; the UI now renders each tab's workspace label alongside its conversation title.
- A pre-session tab has no upstream session connection, so its file tree intentionally stays in a loading state until the first ask creates the session; this avoids showing the server default `cwd` under the newly selected root.
