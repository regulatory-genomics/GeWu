# gewu-protocol

Wire protocol shared by the GeWu server (`gewu-server serve`) and its clients
(the `gewu-client` crate, or any program speaking the protocol directly).

Every message in both directions is a single JSON object: newline-delimited
on TCP (one object per line), one text frame per object on WebSocket.

- **Client → Server:** requests (`ask`, `interrupt`, `list_tools`,
  `list_agents`, `list_sessions`, `get_session`, `delete_session`, `info`,
  `usage`, `ping`) with a client-chosen correlation `id` echoed on every
  reply. Built with `ClientRequest::ask`/`interrupt`/`...` + `to_line()`,
  parsed with `parse_request`.
- **Server → Client:** `ready`, `event`, `result`, `interrupted`, `error`,
  `pong`, `info`, `usage`, `tools`, `agents`, `sessions`, `session` frames
  (`ServerMessage`, serde-tagged with `"type"`). Streamed `event`/`result`
  frames carry `id: null` because they are emitted by the background ask task;
  clients demux them by connection state (one in-flight ask per connection).

`event` payloads are serialized `gewu_core::types::AgentEvent`s.

See `gewu-server/README.md` for the full protocol reference with examples.
