# Findings

## Evidence

### Image Preview

- `gewu-gui/assets/app.js:1352` sends `{op: "read_file"}` for every normal file
  click.
- `gewu-client/src/lib.rs:392-400` accepts only `ServerMessage::FileContent`.
- `gewu-gui/src/session.rs:1457-1472` turns a failed binary read into
  `could not preview file`.
- The existing preview route is restricted by `is_html_path` and returns
  `text/html`, so it cannot serve images.
- `gewu-gui/assets/editor.mjs:33-52` already knows how to render image/video/
  audio resources once it has a usable URL. The missing piece is the Files
  route and MIME-aware action selection.

### Global disabled vs session enabled

- `gewu-server/src/plugins/mod.rs:178-186` stores no `Package` when the global
  enabled override is false.
- `catalog()` reads title/scope/views from `package`, so disabled configured
  plugins are reported with null metadata.
- `gewu-gui/assets/panels.mjs:375-379` renders the session toggle only inside
  `if (plugin.enabled)`, making the desired action impossible from the UI.
- `open_for_session()` cannot load a globally disabled package because it reads
  the live `package` slot; enabling a session requires an explicit state model
  and a package-loading path.

### Plugin context chip

- `gewu-server/src/plugins/actions.rs:179-195` collects up to four active view
  selections from `publish_context`.
- `gewu-server/src/server/mod.rs:2221-2229` injects them into the next user
  prompt as untrusted view data.
- `gewu-gui/src/view.rs:162-186` renders the injected payload as
  `Plugin selection · N` in the user message.
- The behavior is technically intentional, but the wording and prominence are
  misleading; it is context provenance, not a plugin-selection control.

### Current UI density

- `gewu-gui/assets/panels.mjs:325-410` puts status, tool inventory, package/source
  paths, global toggle, session toggle, Open and profile checkboxes in one
  repeated row.
- `gewu-gui/assets/app.css:3240-3248` styles the rows as compact inline text,
  which explains the screenshot's long horizontal controls and weak hierarchy.

## Additional developer-reported issues checked

### Agent gateway coverage

The report is confirmed. `HostAction` has `SetEnabled`, `Calls`, `CallStatus`,
`Cancel` and `FocusView`, while `PluginHost::tools()` registers only five
gateway names. The missing operations are not protocol gaps; they are exposure
and contract gaps.

### State stores

The report is confirmed with one qualification: `plugins/state.json` is the
installation-generation state used by marketplace-managed plugins, while
`~/.gewu/plugin-state.json` is the host's runtime enable override. Both are
legitimate stores, but the current CLI/UI do not explain their scope or update
the live host after bootstrap. The fix should expose one authoritative runtime
state and mark installation state as separate provenance.

### Binding resource

The report is confirmed. `promptResource()` accepts an arbitrary path string,
and `contained()` collapses all failures to `path does not exist`. The host
knows the workspace root and manifest resource types but does not return them in
the open error or discovery payload.

### Manimble source editing

The report is confirmed. The engine compares `req["expected"]` with exact class
text, while the adapter separately validates `source.revision`; the Manimble
skill tells callers to pass the revision as `expected`. `describe` returns only
references, and `scene`/`render` are the current text-bearing operations. This
is a contract/documentation mismatch plus an avoidable expensive read path.

### Session identity and call status

The report is confirmed as an observability problem. Calls are intentionally
owned by `(owner session, workspace)`, so a new connection without the original
session identity cannot see them. The current `call not found` message does not
distinguish invalid ID, wrong session, wrong workspace or expired result.

### Endpoint discovery

The report is confirmed. `gewu up` permits ephemeral ports and prints only the
WebSocket URL to stdout; all three endpoints are available only in server INFO
logs. This is insufficient for a machine client.

### Focus

The missing `focused` field is confirmed. The GUI focus path exists, but
`public_info()` and gateway list output do not expose it, and agent-side open
does not provide a first-class focus operation. The right fix is explicit
session-local focus state, not implicit ordering of instance rows.

## Recommended order

1. Fix global/session state first because it blocks basic plugin activation.
2. Fix media preview because it is a concrete broken path with an existing
   resource-preview primitive.
3. Simplify the panel after the state contract stabilizes.
4. Rename and quiet the context chip, then add stale-context cleanup.
5. In parallel with the first two fixes, repair the agent ACI and Manimble
   source contract; these are independent of the visual redesign and remove
   the highest context/retry cost for agents.
