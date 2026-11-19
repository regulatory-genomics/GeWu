# GeWu Preview usability and plugin-state repair

Status: implemented (P0–P2 slice); Manimble installed package not restaged

## Implemented

- Media preview: `PreviewFile` accepts an allowlisted set of image/PDF/audio/
  video/HTML types; `/preview/{ticket}` serves the right content type (HTML keeps
  the sandbox CSP) and the Files click renders media in the dock overlay.
- Global/session state: `SessionPluginState.enabled` is now `Option<bool>`
  (inherit vs explicit), `catalog()` keeps manifest metadata for disabled
  plugins, `open_for_session` loads a package on demand, and the panel offers
  *Enable in this session* / *Follow global setting*.
- Probes and tests constructed without a config path no longer read or write
  `~/.gewu/plugin-state.json`; the integration tests stopped polluting the real
  user state.
- Agent gateway: added `plugin_set_enabled`, `plugin_set_session`, `plugin_calls`,
  `plugin_call_status`, `plugin_cancel`, `plugin_focus`, plus `async:true` on
  `plugin_call`.
- Resource binding: relative paths are accepted and canonicalised; errors name
  the workspace root and resource type; `plugin_open` returns `workspaceRoot` and
  the normalized binding.
- Focus: `plugin_focus` moves the session focus to an instance, and instance
  listings carry `focused`.
- Endpoint discovery: `gewu up` prints WS/TCP/HTTP/GUI and writes a 0600
  `~/.gewu/server.json` (wildcard binds are published as loopback).
- Panel IA: plugin title/status and primary actions are one row; tool schemas,
  package paths and profile toggles moved under *Advanced*.
- Transcript: `Plugin selection · N` became `App context · N` with an explicit
  note that it is view context, not a command; `enclose` CSS keeps it quiet.
- Manimble: new `source` tool (no render), `expectedText` (with legacy
  `expected` fallback), `oldText/newText` patch mode, `include` field mask on
  `scene`/`render`, actionable stale-source message, and corrected SKILL text.

## Verified

- `cargo test -p gewu-protocol -p gewu-server -p gewu-gui -p gewu`: 497 tests, 0
  failures (28 suites).
- GeWu GUI Playwright: 9/9 including new session-override and image-preview
  cases; `panel-host` updated for the Advanced section.
- `node --test gewu-gui/tests/panels.test.mjs`: 2/2.
- Manimble real-engine suite: 7/7 including `source` without render and a
  minimal patch.
- Live 8889: globally disabled Manimble → *Enable in this session* → *Follow
  global setting* → restore; PNG clicked in Files rendered inline at 1545×897;
  Tachyon still opens.

## Not done / gaps

- The installed Manimble package was not restaged, so the live plugin still runs
  the previous adapter; only the source tree and its tests carry `source`.
  Run `plugins/gewu/package.py` + reinstall to deploy it.
- Reference/resource profile wiring stays out of scope as planned.
- `server.json` can survive a SIGKILL'd process; clients should check `pid`.
- Resource binding errors name the root/type but do not list candidate decks.

## Goal

Make the GeWu Preview surface reliable and understandable for ordinary plugin
use, with four user-visible outcomes:

1. image and other safe media files preview from Files;
2. a globally disabled plugin can be explicitly enabled for the current session;
3. the Plugins panel presents the common actions clearly without exposing
   implementation details by default;
4. plugin-provided view context is represented as a quiet, understandable
   indicator instead of the confusing `Plugin selection` disclosure.

## Priority

- **P0:** global/session runtime state, Manimble source read/edit contract,
  resource binding discovery, and binary/media Preview. These currently block
  reliable recovery or force agents to guess.
- **P1:** agent lifecycle/status/focus gateway and endpoint discovery. These
  remove protocol workarounds and make asynchronous operations recoverable.
- **P2:** Plugins-panel information architecture and the context indicator.
  These improve usability after the state and protocol contracts are stable.

## Root causes found

- Files click always sends `read_file`. The client expects a text-only
  `file_content` response, while binary files are rejected by the server. The
  existing HTML-only `PreviewFile`/`/preview/{ticket}` path is not used for
  images.
- A globally disabled plugin is represented by `package: None` in the live
  registry. `catalog()` therefore loses its title, scope and views, and the UI
  only renders the session toggle when `plugin.enabled` is true. The current
  boolean session state cannot express “inherit global default” versus “explicit
  session override”.
- The Plugins panel is a dense implementation-oriented list: every row mixes
  status, tool schemas, package paths, global controls, session controls and
  running-resource controls.
- `publish_context` stores view selection text. `start_session_turn` appends it
  to the next user prompt under `[Plugin selection context: untrusted view data]`;
  the transcript renders that payload as `Plugin selection · N`. This is useful
  provenance but the current label implies an action the user did not request.

## Scope

### In scope

- `gewu-gui/src/protocol.rs`, `gewu-gui/src/session.rs`,
  `gewu-gui/src/server.rs`, `gewu-gui/src/view.rs`,
  `gewu-gui/assets/app.js`, `gewu-gui/assets/editor.mjs`,
  `gewu-gui/assets/panels.mjs`, `gewu-gui/assets/app.css`;
- `gewu-protocol/src/lib.rs` for file metadata if needed;
- `gewu-server/src/plugins/mod.rs`, `gewu-server/src/plugins/actions.rs`,
  and plugin tests;
- focused GUI, protocol, server, and Playwright regressions.

### Out of scope

- changing plugin adapter APIs;
- changing Tachyon or Manimble application rendering;
- adding a second Canvas implementation;
- redesigning the chat model/tool-selection algorithm;
- reference/resource profile implementation.

## Additional verified findings

- `HostAction` already contains `SetEnabled`, `Calls`, `CallStatus`, `Cancel`,
  `FocusView` and related lifecycle actions, but `gewu-server/src/plugins/tools.rs`
  exposes only five gateway tools: `plugin_list`, `plugin_open`, `plugin_close`,
  `plugin_describe` and `plugin_call`.
- `plugin_list` returns catalog data from the active `Package`; a disabled
  configured plugin has no active package in the registry, so title/scope/views
  become null and no reason or recovery action is returned.
- `gewu plugin enable` updates the installation generation state in
  `plugins/state.json`; the running host reads `~/.gewu/plugin-state.json` and
  stores the result in memory at bootstrap. The two writes have different
  owners and do not update a running host.
- Resource binding currently depends on a free-form prompt in
  `panels.mjs:444-447`; server errors from `contained()` only say `path does not
  exist`. The response does not include workspace root, normalized path,
  accepted resource types or candidate resources.
- Manimble's `edit_source` contract is inconsistent across its schema/skill and
  implementation: the adapter checks `source.revision`, then passes `expected`
  as exact class text to the engine. The skill currently tells agents to pass a
  revision as `expected`, which explains the misleading concurrency error.
- Manimble `describe` returns only source references. `scene` and `render`
  return source text but also render and include object/frame payloads; this is
  the source of the context-heavy read path.
- Plugin calls are asynchronous at the host protocol level, but the ordinary
  agent `plugin_call` gateway awaits the call and exposes no status gateway.
  `CallStatus` is correctly owner/session scoped; a different connection or
  owner receives `call not found` rather than a diagnostic scope mismatch.
- `gewu up` resolves random WebSocket/TCP/HTTP ports, but its stdout reports
  only the WebSocket URL. The server logger reports all three at INFO, which is
  not a reliable discovery contract for agents or CLI clients.
- `public_info()` omits focus. The GUI does issue `FocusView` for a revealed
  webview, but an agent opening an instance has no explicit focus operation or
  observable focused instance in `plugin_list`.

## Tasks and acceptance gates

1. **Media preview contract**
   - Files: `gewu-protocol/src/lib.rs`, `gewu-server/src/server/mod.rs`,
     `gewu-gui/src/protocol.rs`, `gewu-gui/src/session.rs`,
     `gewu-gui/src/server.rs`, `gewu-gui/src/view.rs`,
     `gewu-gui/assets/app.js`.
   - Preserve the existing text editor path.
   - Add a safe, ticketed media preview path for image, audio, video, PDF and
     HTML files, with server-side MIME allowlisting and session containment.
   - Make Files expose Preview only for supported media and keep Download for
     all files.
   - Verify image preview renders an `<img>` and no longer calls text
     `read_file`; verify unsupported/binary files produce a useful message.
   - Verification: `cargo test -p gewu-gui --lib` plus a focused GUI
     file-preview Playwright test.

2. **Global/session plugin state**
   - Files: `gewu-server/src/plugins/mod.rs`,
     `gewu-server/src/plugins/actions.rs`, `gewu-server/src/plugins/tools.rs`,
     `gewu-gui/assets/panels.mjs`, `gewu-protocol/src/plugin.rs`, and plugin
     registry/tool tests.
   - Retain manifest metadata while a plugin is globally disabled.
   - Replace the implicit boolean inheritance with an explicit effective-state
     model that supports global default plus session override.
   - Allow `Enable in this session` for a globally disabled configured plugin;
     keep global disable as the default for sessions without an override.
   - Verify one session can open the plugin while another remains disabled, and
     global re-enable does not erase the explicit session choice.
   - Verification: `cargo test -p gewu-server --test plugin_registry
     --test plugin_tools` and a two-session GUI Playwright test.

3. **Agent plugin lifecycle gateway**
   - Files: `gewu-server/src/plugins/tools.rs`,
     `gewu-server/src/plugins/actions.rs`, `gewu-protocol/src/plugin.rs`, and
     gateway contract tests.
    - Expose `plugin_set_enabled` and `plugin_set_session` with authoritative
      effective state and typed reasons;
     expose `plugin_call_status`, `plugin_calls`, `plugin_cancel` and
     `plugin_focus` where the existing HostAction already supports them.
   - Keep async calls asynchronous for agents: return `callId`, status and
     `next_actions` instead of blocking the tool turn.
   - Make cross-session/workspace failures say which identity is required and
     how to retry, without leaking another session's data.
   - Verification: gateway schema tests plus a real fixture turn that opens,
     calls, polls and cancels a plugin operation.

4. **Resource binding discovery**
   - Files: `gewu-server/src/plugins/mod.rs`,
     `gewu-server/src/plugins/actions.rs`, `gewu-server/src/plugins/package.rs`,
     `gewu-server/src/plugins/tools.rs`, `gewu-gui/assets/panels.mjs`, and
     `gewu-protocol/src/plugin.rs`.
   - Include workspace identity/root, resource types and candidate resource
     hints in plugin discovery; do not expose secrets or unrelated filesystem
     data.
   - Accept a safe relative path convenience form only at the host boundary,
     normalize it to a canonical `workspace:///` URI, and return both the
     normalized URI and absolute path on success.
   - Change path errors to include workspace root, normalized candidate and
     accepted resource type, with a bounded candidate list when available.
   - Verification: resource/path unit tests plus GUI coverage for a valid deck,
     a missing path and a wrong resource type.

5. **Manimble source-edit contract**
   - Files: `/data3/litian/public_project/Manimble/engine/manimble/core.py`,
     `/data3/litian/public_project/Manimble/plugins/gewu/adapter.py`,
     `/data3/litian/public_project/Manimble/plugins/gewu/tools.schema.json`,
     `/data3/litian/public_project/Manimble/plugins/gewu/skills/manimble/SKILL.md`,
     and Manimble engine/adapter tests.
   - Add a read-only source operation or a bounded `describe` source field so
     agents can retrieve class text without rendering objects.
   - Rename the exact-text compare field to `expectedText` or introduce
     `expectedRevision` backed by one canonical hash contract; do not keep a
     field called `expected` whose meaning changes across layers.
    - Add a minimal `oldText/newText` patch mode for small source edits, preserve
      the existing full replacement mode for compatibility, and return the
      engine's original structured error message.
    - Add bounded `include`/field-mask behavior to `scene` and `render`, with
      object/frame detail omitted by default when an agent only needs source or
      slide status.
    - Document the minimum deck shape (`slides.py` plus `order.txt`) and add a
      small scaffold path so an agent does not need to infer it from fixtures.
   - Update Manimble's schema, adapter, engine and skill documentation together.
   - Verification: `python -m unittest discover -s plugins/gewu/tests`, focused
     source-read/patch tests, and a live `edit_source` call using bounded read
     output.

6. **Focus and instance observability**
   - Files: `gewu-server/src/plugins/actions.rs`,
     `gewu-server/src/plugins/mod.rs`, `gewu-server/src/plugins/tools.rs`,
     `gewu-gui/assets/panels.mjs`, and plugin action tests.
   - Add `focused` to instance/list output and make opening a webview or an
     explicit `plugin_focus` operation update the session focus.
   - Keep focus session-local and ensure closing/detaching the focused view
     selects a deterministic remaining view or explicitly clears focus.
   - Verification: plugin action tests and a GUI two-instance focus-switch
     regression that checks agent context follows the focused view.

7. **Endpoint discovery**
   - Files: `gewu/src/up.rs`, `gewu-server/src/server/mod.rs`, and runtime
     endpoint tests.
   - Write a redacted, permissioned runtime endpoint record containing PID,
     WebSocket, TCP and HTTP addresses, or provide a machine-readable stdout
     record for `gewu up`.
   - Preserve random-port behavior; the change is discovery, not fixed ports.
   - Verification: `cargo test -p gewu --bin gewu` plus an ephemeral-port
     startup test that parses the endpoint record.

8. **Preview information architecture**
   - Files: `gewu-gui/assets/panels.mjs`, `gewu-gui/assets/app.css`,
     `gewu-gui/tests/panels.test.mjs` and relevant Playwright specs.
   - Separate common actions from advanced diagnostics.
   - Show plugin title/status, session action and Open in the primary row.
   - Move tool schemas, package paths and profile checkboxes behind deliberate
     expandable sections.
   - Keep running resource views visible as child rows with clear close/stop
     actions.
   - Verification: GUI unit tests and the full GeWu GUI Playwright suite at
     desktop and narrow-dock widths.

9. **Context indicator**
   - Files: `gewu-gui/src/view.rs`, `gewu-server/src/plugins/actions.rs`,
     `gewu-server/src/server/mod.rs`, `gewu-gui/assets/app.css`, and transcript
     tests.
   - Rename the disclosure to `App context` or equivalent and explain that it
     is view context supplied to the next agent turn.
   - Render it collapsed and visually subordinate; do not imply that the user
     selected a plugin.
   - Clear or exclude stale context when its view is detached, closed or
     disabled.
   - Verification: transcript HTML tests plus a detach/close/next-turn live
     regression showing no stale context chip.

10. **Release gate**
   - Files: all files changed by Tasks 1–9 and their tests.
   - Run focused Rust tests, GUI unit tests, the full GeWu Playwright suite and
     `git diff --check`.
   - Reproduce the four original symptoms in the live 8889 GUI and verify the
     consumer-visible behavior, not only server responses.
   - Verification: focused Rust/Manimble suites, full GUI Playwright suite,
     `git diff --check`, and live 8889 smoke checks.
