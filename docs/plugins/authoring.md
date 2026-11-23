# Write a GeWu adapter

Ship a `plugin.json`, an executable declared by `backend.config.entry`, optional static assets under `web/`, and the app's normal dependencies. Use the [v1 protocol](protocol-v1.md); Rust can use `gewu_protocol::plugin` and Python can vendor `sdk/python/gewu_plugin.py` into the staged package. That helper has no third-party runtime dependencies. Launch native programs with explicit argv. The host starts the adapter in the workspace with a minimal environment (`PATH`, `HOME`, locale and temp settings); resolve credentials before activation and pass app-specific settings explicitly. Do not return credentials in public results, descriptions, progress, or saved state.

## Browser-native application views

`webview` runs a plugin's own static page in an opaque-origin frame. A whole web
application that needs its own origin — its own API, `Range` media, SSE or a
service worker — declares `kind: "webapp"` instead:

```json
{"id":"studio","title":"Studio","kind":"webapp","placement":"main",
 "serviceId":"studio","entryPath":"/","resourceTypes":["example.project"],
 "stateSchema":{"type":"object"}}
```

- `serviceId` must be an HTTP `serviceEndpoints` entry from `activate`;
  `entryPath` is an origin-relative path and may not carry a query, fragment,
  backslash or `..` segment.
- On attach the host returns `app.relayPath`, `app.relayKey` and `app.entryPath`
  in the host-only attach result. The GUI turns that into a browser URL on the
  configured application origin; the relay key and the service's private headers
  never reach the model or the page.
- The operator configures the origins:
  `gewu gui --public-origin https://gewu.example.com --plugin-app-origin-template 'https://{view}.apps.gewu.example.com'`.
  Remote deployments require HTTPS and same-site origins; `{view}` must be the
  whole first hostname label. Local development can use
  `http://gui.gewu.localhost:8430` and `http://{view}.apps.gewu.localhost:8430`.
- The page boots from `/_gewu/bootstrap`, exchanges the one-time fragment
  credential for a host-only `HttpOnly` cookie, then hides in the view's own
  origin. The bridge handshake (`gewu.view.ready` → `gewu.view.bound`) checks the
  exact origin, view id and a per-attach nonce; state, selection and `callTool`
  work as in a webview.
- Only the current project's routes are reachable through the relay: static
  assets, the project API, previews/media, history, registry and events. Desktop
  launch, share/publish, other projects and the GUI's own socket/config are not.


## Agent tool discovery

Declare native tool names in the adapter; the host generates collision-safe
`plg_…` typed names uniformly for every plugin ID. Discover tools and schemas
instead of constructing host names. `plugin_describe` and `plugin_call` accept
the native names and enforce the same session tool profiles as typed calls.
Profile choices apply to all plugin scopes and persist per conversation.

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

`CallContext.callId` is always unique to the adapter call. `runId` is optional;
adapters must accept it being absent. Native Agent tool calls are cancelled when
their owning turn is interrupted; explicitly asynchronous gateway calls have
their own call IDs and are inspected or cancelled through the plugin gateway.

## Source-aware development

An optional `sources` map in a local plugin config entry identifies the adapter
and the software it connects to. Paths resolve relative to the config file:

```json
{
  "plugins": {
    "manimble": {
      "package": "/path/to/staged-manimble",
      "enabled": true,
      "sources": {
        "adapter": "/path/to/Manimble/plugins/gewu",
        "application": "/path/to/Manimble"
      }
    }
  }
}
```

Both the Plugins panel and the agent's `plugin_list` expose these paths, together
with the loaded package path. This is source discovery, not a filesystem grant:
normal workspace/tool permissions still apply. Put build/test/update instructions
in the plugin's existing skill so the agent can follow the software's native
development workflow. Source paths are host-local config, not portable package metadata.

For a development package, edit source, run its tests, then stage/build into the
configured package location. Use **Reload view** in Preview to read the updated
web assets while retaining the current native instance. Adapter or native Python changes require
Stop and Open to restart the instance; native compiled software also needs its normal
build. Manifest/backend/settings changes require restarting GeWu to reload the
configuration. A registered source directory is not itself the installed package.
GeWu does not watch source paths or replace running code automatically.

## Test

Run `scripts/plugin-conformance.py` against the staged package. A browser spec can export `register({test,expect,startFixture})`; GeWu's `application-plugin.spec.mjs` loads that spec with `GEWU_APP_SPEC` and `GEWU_APP_PLUGIN`. `startFixture({settings})` starts the actual Rust server and GUI on loopback, returning `url`, a temporary workspace `root`, and `close()`. Keep native engine fixtures small and pass configured dependencies explicitly. Package smoke tests should fail clearly when a required binary is missing.
