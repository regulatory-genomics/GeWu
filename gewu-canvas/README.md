# gewu-canvas

GeWu's host-side runtime for agent-controllable canvas surfaces. Pair with
[tachyon-canvas](../../../tachyon-canvas) for genome canvas rendering.

This crate provides:

- **Generic canvas types** (`CanvasState`, `CanvasMode`, `CanvasKind`,
  `CanvasStatePatch`) that mirror `tachyon-canvas/contracts/*.ts`.
- **Genome-specific payload types** (`GenomeCanvasState`, `SampleSpec`,
  `TrackSpec`) for typed access from server-side tools.
- **In-memory `CanvasManager`** (per-session map of canvases).
- **Six `canvas.*` agent tools**: `open`, `close`, `focus`, `set_state`,
  `get_state`, `list`.
- **Five `genome.*` agent tools** (HTTPS clients for `tachyon-browser`):
  `inspect_region`, `compare_samples`, `local_qc`, `scan_candidates`,
  `explain_region`.

## How it plugs into GeWu

`gewu-web/src/state.rs::AppState::new` constructs a `CanvasManager` and
wraps it in the six canvas tools. When `TACHYON_BROWSER_URL` is set, the
five genome tools are also registered. All eleven tools are appended to
the per-session `mcp_tools` so any specialist can use them.

The `CanvasManager` is also exposed on `AppState.canvases` and consumed by
the inverse-channel REST endpoints
(`/api/sessions/:sid/canvas/...`), letting the browser push user-driven
state changes back to the server.

## SSE bridge

Per `tachyon-canvas/docs/sse-bridge.md`, canvas events ride on the existing
GeWu tool-event stream:

- `canvas.open` returns `{event:"canvas:open", ...}` from `start_metadata`,
  arriving in `tool_start.metadata` on the browser.
- `canvas.set_state` / `canvas.close` / `canvas.focus` write
  `{event:"canvas:patch|close|focus", ...}` into `ToolResult.output`,
  parsed by the existing `tool_finish` handler.

No changes to `gewu_core::types::AgentEvent` are required.

## Tests

```
cargo test -p gewu-canvas
```

Exercises state merge rules, manager session-scoping, tool start_metadata
emission, and the genome-tools registration path.
