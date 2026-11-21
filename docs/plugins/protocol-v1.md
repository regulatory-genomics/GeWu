# GeWu adapter protocol v1

The machine contract is [`gewu-protocol/schema/plugin.v1.schema.json`](../../gewu-protocol/schema/plugin.v1.schema.json) (JSON Schema 2020-12). The root validates `plugin.json`; select `#/$defs/HostFrame` or `#/$defs/AdapterFrame` to validate the transport. The [JSONL examples](../../gewu-protocol/tests/fixtures/plugin-v1/) cover each operation, including out-of-order cancellation and private process control.

## Package and transport

`plugin.json` declares `apiVersion: "gewu.plugin/v1"`, package identity, version, scope (`workspace` or `resource`), a `gewu-stdio` backend with a relative executable `entry` and optional argv `args`, optional `views`, and a `settingsSchema`. Install only trusted adapter packages. The adapter is a local executable, not an OS sandbox. Host validates canonical entry and view paths against the package root. Settings are passed in `activate`, never in the model's tool arguments.

Read and write exactly one UTF-8 JSON object per LF-delimited line (at most 1 MiB excluding LF). Keep stdout for protocol and stderr for logs. Each host request is `{"id":"r1","op":"…","params":{…}}`. An adapter reply is `type: "result"`, `"error"`, or `"event"`, echoing the request ID. A request has exactly one terminal reply. Progress may precede the terminal. Read control requests while work is running; replies to different IDs may arrive in any order. Missing or malformed replies do not mean success. Unknown ops or protocol versions, repeated pending IDs, and invalid frames must be rejected.

| Operation | Parameters | Successful result |
| --- | --- | --- |
| `activate` | Version, instance/generation, workspace ID/root, optional binding resource, settings | `{tools, serviceEndpoints}`, only after the app is ready |
| `call` | `{tool, args, context}` | `ToolOutput` (`ok:true` with `data`, or `ok:false` with `error`; both carry `resources`) |
| `cancel` | `{callId}` | `{state:"requested"\|"already_settled"\|"not_found"}` |
| `resolve` | A `ResourceRef` directly | A private workspace file source or a path relative to a registered service origin |
| `deactivate` | `{reason}` | `{stopped:true}` after cleanup, followed by process exit |

`type:"error"` is a protocol/request failure. A completed but unsuccessful tool uses a normal `type:"result"` with `ok:false`; changes and resources reported there must be confirmed partial effects. `CANCELLED` means work actually stopped. A cancel acknowledgement does not settle the original call. If a native operation's outcome cannot be confirmed, use `OUTCOME_UNKNOWN` with `retryable:false` and do not automatically replay writes. Report `CLEANUP_INCOMPLETE` when cleanup cannot be verified. Generations fence stale calls and resources. `bindingResource` identifies the document/deck owning an instance, whereas `resource` is its current content (such as a preview). `viewId` and `viewEpoch` are supplied together, by the host.

### Optional HTTP data plane

`serviceEndpoints` stays private to the host. Each descriptor registers an origin (no userinfo, path, query or fragment), optional private headers, and explicit `routes` (path prefix and methods). Omitting `routes` grants no browser service access. To avoid ambiguous prefix matches, the host checks exact paths or whole path components. `resolve` must never return an arbitrary URL. A file source is relative to the bound workspace (absolute paths must canonicalize within it). A service source has a registered `serviceId` and an origin-relative path. Views use host-issued tickets rather than credentials.

### Optional private process control

For a child requiring its own process group, send `type:"event"` (on a pending request) with `event: {type:"process.started",pid,pgid}`. Hold the child at a startup barrier until the host sends `{type:"process.accepted",id,pid,pgid}`. This control frame is not a request and does not have its own reply. If registration fails or times out, stop and reap the child. Report `process.exited` with the same process identity after the group is done. Do not daemonize; attached software is not owned by GeWu.

## Adapter validation

With Python 3.11+ and `jsonschema` installed, use:

```sh
python scripts/plugin-conformance.py --package DIR --settings settings.json \
  --workspace DIR --scenario scenario.json --schema-root gewu-protocol/schema
```

The scenario is an array of `{"send": <host frame>}` / `{"expect": <adapter frame or subset>}` steps. Exact string values `${workspace}`, `${package}`, and `${settings}` are substituted; `${settings}` becomes the settings JSON object. Expected object keys are matched recursively; arrays must match in order. Include an activate handshake and a final deactivate handshake. Exit 0 means all frames validated, requests settled, and the adapter exited cleanly; exit 1 is a verification failure. See `gewu-server/tests/fixtures/plugin-adapter` for a runnable fixture.
