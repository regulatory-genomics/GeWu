# Findings

- The GUI currently has a top tab bar and a right file/tools sidebar, but no session-management sidebar.
- `TabInfo.workspace` is already available to render directory groups without changing the upstream protocol.
- The existing `CreateTab` command accepts workspace ID and path, so a group-level `+` can create a preselected blank tab directly.
- The existing `send_tabs` path is the central update point for tab changes; a session-sidebar update can be emitted there alongside `Tabs`.
- The left sidebar is hidden below 800px while the existing right file sidebar keeps its overlay behavior; this avoids squeezing the transcript on narrow screens.
- Workspace groups include configured roots even when they have no session, so each group can create its first session directly with `+`.
