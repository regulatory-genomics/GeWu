# Write a GeWu adapter

Ship a `plugin.json`, an executable declared by `backend.config.entry`, optional static assets under `web/`, and the app's normal dependencies. Use the [v1 protocol](protocol-v1.md); Rust can use `gewu_protocol::plugin` and Python can vendor `sdk/python/gewu_plugin.py` into the staged package. That helper has no third-party runtime dependencies. Launch native programs with explicit argv. The host starts the adapter in the workspace with a minimal environment (`PATH`, `HOME`, locale and temp settings); resolve credentials before activation and pass app-specific settings explicitly. Do not return credentials in public results, descriptions, progress, or saved state.

## Browser bridge

Load the application in GeWu's opaque-origin frame. After the view JS is ready, call `parent.postMessage({type:'gewu.view.ready'}, '*')`. Listen for `type:'gewu.view.bound'` from `parent` with precisely one transferred `MessagePort`; it includes `context` (`workspaceId`, `instanceId`, `viewId`, `viewEpoch`, `bindingResource`, `resource`, `binding`, `state`). `binding` is private adapter data returned by `view.attach`, provided only to the bound view. `view.detach` is optional. Both tools must have `exposeToAgent:false` and accept `{}`; the metadata is in CallContext.

Send `{id,method,params,viewEpoch}` through the dedicated port. Each reply has `{id,result}` or `{id,error}`. Methods:

| Method | Params | Result |
| --- | --- | --- |
| `getContext` | `{}` | Current view context |
| `callTool` | `{tool,args}` | `{callId}`; accepted, still running |
| `getCall` / `cancelCall` | `{callId}` | State/result or cancellation acknowledgement |
| `resolveResource` | `{resource}` | `{url}` for a bounded ticket |
| `openResource` | `{resource}` | Opens a view or built-in preview |
| `publishContext` | `{summary}` | Latest selection for the next user turn, max 2 KiB |
| `saveState` | `{state}` | Validates and remembers state in the user's layout |
| `requestService` | `{serviceId,path,method}` | `{url}` for this service request |

Use the URL returned by the host rather than constructing ticket paths. For HTTP bytes, fetch the URL with `credentials:'omit'`, the approved method, and optional body; read SSE with streaming `fetch`. The service host and private headers stay on the server. Tickets are short-lived, and a closed view or adapter generation cannot mint or redeem old tickets. Re-resolve an expired ticket. For assets use relative URLs to files under `web/` and external JS modules; the frame does not run inline scripts.

## Test

Run `scripts/plugin-conformance.py` against the staged package. A browser spec can export `register({test,expect,startFixture})`; GeWu's `application-plugin.spec.mjs` loads that spec with `GEWU_APP_SPEC` and `GEWU_APP_PLUGIN`. `startFixture({settings})` starts the actual Rust server and GUI on loopback, returning `url`, a temporary workspace `root`, and `close()`. Keep native engine fixtures small and pass configured dependencies explicitly. Package smoke tests should fail clearly when a required binary is missing.
