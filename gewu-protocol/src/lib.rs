//! Shared wire protocol for the GeWu server (`gewu-server serve`) and its
//! clients (the `gewu-client` crate, or any program speaking the protocol
//! directly).
//!
//! Every message in both directions is a single JSON object. On the TCP
//! transport messages are newline-delimited (one JSON object per line); on
//! WebSocket each message is one text frame containing one JSON object.
//!
//! # Client → Server
//!
//! ```json
//! {"id": "1", "op": "ask", "prompt": "List the rust files", "session_id": "optional", "agent": "build"}
//! {"id": "2", "op": "interrupt"}
//! {"id": "3", "op": "list_tools"}
//! {"id": "4", "op": "list_agents"}
//! {"id": "5", "op": "ping"}
//! ```
//!
//! `id` is a client-chosen correlation ID (any JSON value) echoed on every
//! reply. `session_id` and `agent` are optional and only honored when the
//! connection's session is first created (by the first request that needs
//! one); later requests reuse the session.
//!
//! # Server → Client
//!
//! ```json
//! {"type": "ready", "id": "1", "request_id": "c3f1...", "session_id": "s1", "agent": "build"}
//! {"type": "event", "id": "1", "request_id": "c3f1...", "event": {"type": "content_delta", "delta": "Hello"}}
//! {"type": "event", "id": "1", "request_id": "c3f1...", "event": {"type": "tool_start", "tool": "bash", "input": "{...}"}}
//! {"type": "result", "id": "1", "request_id": "c3f1...", "session_id": "s1", "response": "Done."}
//! {"type": "interrupted", "id": "1", "request_id": "c3f1..."}
//! {"type": "error", "id": "1", "error": "an ask is already in progress"}
//! {"type": "pong", "id": "5"}
//! {"type": "tools", "id": "3", "tools": ["read_file", "bash", "..."]}
//! {"type": "agents", "id": "4", "agents": ["build", "explore", "general"]}
//! ```
//!
//! `event` payloads are serialized [`AgentEvent`]s (tagged with `"type"`:
//! `content_delta`, `reasoning_delta`, `tool_start`, `tool_finish`,
//! `finish`, `error`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use gewu_core::types::{AgentEvent, ContentPart, FunctionCall, Message, ToolCall};

/// Operations a client can request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Run an agent turn with the given prompt; events stream back.
    Ask { prompt: String },
    /// Interrupt the currently running `ask` on this connection, if any.
    Interrupt,
    /// List the tool names available to the agent.
    ListTools,
    /// List the available agent profiles (build/explore/general).
    ListAgents,
    /// List persisted conversation sessions, newest first.
    ListSessions,
    /// Load one persisted session's messages. The session id is carried in
    /// [`ClientRequest::session_id`].
    GetSession,
    /// Delete one persisted session's messages. The session id is carried in
    /// [`ClientRequest::session_id`].
    DeleteSession,
    /// Report the model and provider the server was started with.
    Info,
    /// Report the current session's context usage (estimated tokens vs the
    /// agent's context limit). Creates the session when `session_id` is set.
    Usage,
    /// Liveness check.
    Ping,
}

/// A request sent by a client, or received by the server.
///
/// Construct requests with the associated functions (`ask`, `interrupt`, ...)
/// and the builder-style `with_session` / `with_agent`, then serialize with
/// [`ClientRequest::to_line`] (TCP) or `serde_json::to_string` (WebSocket).
#[derive(Debug, Clone, PartialEq)]
pub struct ClientRequest {
    /// Client-assigned correlation ID echoed on every reply.
    pub id: Value,
    /// The requested operation.
    pub op: Op,
    /// Optional session ID, honored when the connection session is created.
    pub session_id: Option<String>,
    /// Optional agent profile name, honored when the connection session is
    /// created (defaults to `build`).
    pub agent: Option<String>,
}

/// Serialize in the protocol's wire format, e.g.
/// `{"id":1,"op":"ask","prompt":"hi","session_id":"s","agent":"build"}`.
impl Serialize for ClientRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let extra = self.session_id.is_some() as usize
            + self.agent.is_some() as usize
            + matches!(self.op, Op::Ask { .. }) as usize;
        let mut map = serializer.serialize_map(Some(2 + extra))?;
        map.serialize_entry("id", &self.id)?;
        match &self.op {
            Op::Ask { prompt } => {
                map.serialize_entry("op", "ask")?;
                map.serialize_entry("prompt", prompt)?;
            }
            Op::Interrupt => map.serialize_entry("op", "interrupt")?,
            Op::ListTools => map.serialize_entry("op", "list_tools")?,
            Op::ListAgents => map.serialize_entry("op", "list_agents")?,
            Op::ListSessions => map.serialize_entry("op", "list_sessions")?,
            Op::GetSession => map.serialize_entry("op", "get_session")?,
            Op::DeleteSession => map.serialize_entry("op", "delete_session")?,
            Op::Info => map.serialize_entry("op", "info")?,
            Op::Usage => map.serialize_entry("op", "usage")?,
            Op::Ping => map.serialize_entry("op", "ping")?,
        }
        if let Some(session_id) = &self.session_id {
            map.serialize_entry("session_id", session_id)?;
        }
        if let Some(agent) = &self.agent {
            map.serialize_entry("agent", agent)?;
        }
        map.end()
    }
}

impl ClientRequest {
    /// Build an `ask` request.
    pub fn ask(id: impl Into<Value>, prompt: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            op: Op::Ask {
                prompt: prompt.into(),
            },
            session_id: None,
            agent: None,
        }
    }

    /// Attach an explicit session id (honored when the connection session is
    /// first created, e.g. to resume a previous conversation).
    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Attach an explicit agent profile name (honored when the connection
    /// session is first created; defaults to `build`).
    pub fn with_agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = Some(agent.into());
        self
    }

    /// Build an `interrupt` request.
    pub fn interrupt(id: impl Into<Value>) -> Self {
        Self::simple(id, Op::Interrupt)
    }

    /// Build a `list_tools` request.
    pub fn list_tools(id: impl Into<Value>) -> Self {
        Self::simple(id, Op::ListTools)
    }

    /// Build a `list_agents` request.
    pub fn list_agents(id: impl Into<Value>) -> Self {
        Self::simple(id, Op::ListAgents)
    }

    /// Build a `list_sessions` request.
    pub fn list_sessions(id: impl Into<Value>) -> Self {
        Self::simple(id, Op::ListSessions)
    }

    /// Build a `get_session` request for one stored session.
    /// Build a `get_session` request (one stored conversation).
    pub fn get_session(id: impl Into<Value>, session_id: impl Into<String>) -> Self {
        Self::simple(id, Op::GetSession).with_session(session_id)
    }

    /// Build a `delete_session` request (remove a stored conversation). The
    /// session id is carried in `session_id`, as with `get_session`.
    pub fn delete_session(id: impl Into<Value>, session_id: impl Into<String>) -> Self {
        Self::simple(id, Op::DeleteSession).with_session(session_id)
    }

    /// Build a `ping` request.
    pub fn ping(id: impl Into<Value>) -> Self {
        Self::simple(id, Op::Ping)
    }

    /// Build an `info` request (server model + provider).
    pub fn info(id: impl Into<Value>) -> Self {
        Self::simple(id, Op::Info)
    }

    /// Build a `usage` request (context tokens vs limit). Pass the session id
    /// to have the server create/load that session before measuring.
    pub fn usage(id: impl Into<Value>) -> Self {
        Self::simple(id, Op::Usage)
    }

    fn simple(id: impl Into<Value>, op: Op) -> Self {
        Self {
            id: id.into(),
            op,
            session_id: None,
            agent: None,
        }
    }

    /// Serialize to a single-line JSON string (for the TCP transport). For
    /// WebSocket, `serde_json::to_string` produces the same bytes without a
    /// trailing newline; `to_line` also does not append one.
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| r#"{"id":null,"op":"ping"}"#.to_string())
    }
}

/// Errors raised while parsing or handling a client request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError {
    /// The line could not be parsed as JSON or did not match the protocol.
    Invalid(String),
    /// The operation itself failed (e.g. unknown agent profile, storage error).
    Failed(String),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestError::Invalid(msg) => write!(f, "invalid request: {}", msg),
            RequestError::Failed(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for RequestError {}

/// Parse one client message (a JSON object) into a [`ClientRequest`].
pub fn parse_request(line: &str) -> Result<ClientRequest, RequestError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|e| RequestError::Invalid(format!("not valid JSON: {}", e)))?;

    let obj = value
        .as_object()
        .ok_or_else(|| RequestError::Invalid("expected a JSON object".to_string()))?;

    let id = obj
        .get("id")
        .cloned()
        .ok_or_else(|| RequestError::Invalid("missing required field 'id'".to_string()))?;

    let op_str = obj
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| RequestError::Invalid("missing required field 'op'".to_string()))?;

    let op = match op_str {
        "ask" => {
            let prompt = obj
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    RequestError::Invalid("op 'ask' requires a 'prompt' string".to_string())
                })?
                .to_string();
            Op::Ask { prompt }
        }
        "interrupt" => Op::Interrupt,
        "list_tools" => Op::ListTools,
        "list_agents" => Op::ListAgents,
        "list_sessions" => Op::ListSessions,
        "get_session" => {
            if obj.get("session_id").and_then(Value::as_str).is_none() {
                return Err(RequestError::Invalid(
                    "op 'get_session' requires a 'session_id' string".to_string(),
                ));
            }
            Op::GetSession
        }
        "delete_session" => {
            if obj.get("session_id").and_then(Value::as_str).is_none() {
                return Err(RequestError::Invalid(
                    "op 'delete_session' requires a 'session_id' string".to_string(),
                ));
            }
            Op::DeleteSession
        }
        "ping" => Op::Ping,
        "info" => Op::Info,
        "usage" => Op::Usage,
        other => {
            return Err(RequestError::Invalid(format!("unknown op '{}'", other)));
        }
    };

    let session_id = obj
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let agent = obj.get("agent").and_then(Value::as_str).map(str::to_string);

    Ok(ClientRequest {
        id,
        op,
        session_id,
        agent,
    })
}

/// Extract the client request id from a raw (possibly malformed) message, so
/// error replies can echo it back. Returns `None` when the message is not a
/// JSON object carrying an `id` field.
pub fn extract_id(line: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(line).ok()?;
    value.get("id").cloned()
}

/// Serialize an [`AgentEvent`] as the nested JSON used inside `event` frames.
pub fn agent_event_to_value(event: &AgentEvent) -> Value {
    serde_json::to_value(event).unwrap_or_else(|_| {
        serde_json::json!({
            "type": "error",
            "error": "failed to serialize agent event"
        })
    })
}

/// Summary of one persisted conversation session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Server session id, usable to resume the conversation.
    pub session_id: String,
    /// Number of stored messages.
    pub message_count: i64,
    /// Human-readable time of the most recent message.
    pub updated_at: String,
}

/// Messages the server sends to the client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Sent when an `ask` begins; carries the server-assigned request id
    /// and the session handling the request.
    Ready {
        /// Echo of the client request id.
        id: Value,
        /// Server-assigned request id for this `ask`.
        request_id: String,
        /// Session id used for this ask.
        session_id: String,
        /// Agent profile handling the ask.
        agent: String,
    },
    /// A streamed [`AgentEvent`] from the running agent.
    ///
    /// `id` is always `null` here: streamed events come from a background
    /// ask task, not from the request loop, so clients demux them by
    /// connection state (one in-flight ask per connection) rather than by id.
    Event {
        /// Always `null` (see above); present for wire compatibility.
        id: Value,
        /// Server-assigned request id for the running `ask`.
        request_id: String,
        /// The agent event.
        event: Value,
    },
    /// Final result of a successful `ask`.
    Result {
        /// Always `null` (see [`ServerMessage::Event`]).
        id: Value,
        /// Server-assigned request id for the completed `ask`.
        request_id: String,
        /// Session id handling the conversation.
        session_id: String,
        /// The agent's final response text.
        response: String,
    },
    /// The running `ask` was interrupted, or no ask was running.
    Interrupted {
        /// Echo of the client request id, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
        /// Request id of the interrupted ask, when one was running.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// An error occurred while handling the request.
    Error {
        /// Echo of the client request id, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
        /// Human-readable error description.
        error: String,
    },
    /// Reply to a `ping` request.
    Pong {
        /// Echo of the client request id.
        id: Value,
    },
    /// Reply to an `info` request: the model/provider the server runs.
    Info {
        /// Echo of the client request id.
        id: Value,
        /// Model name (e.g. `gpt-4o`).
        model: String,
        /// Provider name (e.g. `openai`).
        provider: String,
    },
    /// Reply to a `usage` request: the session's context usage.
    Usage {
        /// Echo of the client request id.
        id: Value,
        /// Estimated tokens in the conversation (the same estimate compaction
        /// uses). `0` when the connection has no session to measure.
        used_tokens: u64,
        /// The agent's context window in tokens. `0` when unknown.
        context_limit: u64,
    },
    /// Reply to a `list_tools` request.
    Tools {
        /// Echo of the client request id.
        id: Value,
        /// Names of the tools available to the agent.
        tools: Vec<String>,
    },
    /// Reply to a `list_agents` request.
    Agents {
        /// Echo of the client request id.
        id: Value,
        /// Available agent profile names.
        agents: Vec<String>,
    },
    /// Reply to a `list_sessions` request.
    Sessions {
        /// Echo of the client request id.
        id: Value,
        /// Stored sessions, newest first.
        sessions: Vec<SessionSummary>,
    },
    /// Reply to a `get_session` request.
    Session {
        /// Echo of the client request id.
        id: Value,
        /// Session the messages belong to.
        session_id: String,
        /// Stored messages, oldest first.
        messages: Vec<Message>,
    },
    /// Reply to a `delete_session` request: the session's messages were
    /// removed from the store.
    Deleted {
        /// Echo of the client request id.
        id: Value,
        /// Session that was deleted.
        session_id: String,
    },
}

impl ServerMessage {
    /// Serialize to a single-line JSON string (for the TCP transport).
    pub fn to_line(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| r#"{"type":"error","error":"serialization failed"}"#.to_string())
    }

    /// The message's `"type"` tag, e.g. `"event"`, `"result"`, `"error"`.
    pub fn kind(&self) -> &'static str {
        match self {
            ServerMessage::Ready { .. } => "ready",
            ServerMessage::Event { .. } => "event",
            ServerMessage::Result { .. } => "result",
            ServerMessage::Interrupted { .. } => "interrupted",
            ServerMessage::Error { .. } => "error",
            ServerMessage::Pong { .. } => "pong",
            ServerMessage::Info { .. } => "info",
            ServerMessage::Usage { .. } => "usage",
            ServerMessage::Tools { .. } => "tools",
            ServerMessage::Agents { .. } => "agents",
            ServerMessage::Sessions { .. } => "sessions",
            ServerMessage::Session { .. } => "session",
            ServerMessage::Deleted { .. } => "deleted",
        }
    }

    /// The client correlation id carried by this message, when present.
    ///
    /// Streamed ask frames (`event`, `result`) carry `id: null` because they
    /// are emitted by the background ask task; those return `None` here.
    pub fn correlation_id(&self) -> Option<&Value> {
        match self {
            ServerMessage::Ready { id, .. }
            | ServerMessage::Pong { id }
            | ServerMessage::Info { id, .. }
            | ServerMessage::Usage { id, .. }
            | ServerMessage::Tools { id, .. }
            | ServerMessage::Agents { id, .. }
            | ServerMessage::Sessions { id, .. }
            | ServerMessage::Session { id, .. }
            | ServerMessage::Deleted { id, .. } => Some(id),
            ServerMessage::Interrupted { id, .. } | ServerMessage::Error { id, .. } => id.as_ref(),
            ServerMessage::Event { .. } | ServerMessage::Result { .. } => None,
        }
    }

    /// The terminal-ness of this message for an in-flight `ask`:
    /// `Some(true)` for `result`, `Some(false)` for `error`, and `None` for
    /// messages that do not end an ask (`ready`, `event`, `interrupted`).
    ///
    /// Clients treat `result`/`error` as the end of an ask; `interrupted`
    /// arrives on the interrupt request's own correlation id, so it does not
    /// terminate the ask's event stream by itself.
    pub fn is_ask_terminal(&self) -> Option<bool> {
        match self {
            ServerMessage::Result { .. } => Some(true),
            ServerMessage::Error { .. } => Some(false),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ask_request() {
        let req = parse_request(r#"{"id": 7, "op": "ask", "prompt": "hi"}"#).unwrap();
        assert_eq!(req.id, Value::Number(7.into()));
        assert_eq!(
            req.op,
            Op::Ask {
                prompt: "hi".to_string()
            }
        );
        assert_eq!(req.session_id, None);
        assert_eq!(req.agent, None);
    }

    #[test]
    fn parses_optional_session_and_agent() {
        let req = parse_request(
            r#"{"id": "a", "op": "ask", "prompt": "hi", "session_id": "s1", "agent": "explore"}"#,
        )
        .unwrap();
        assert_eq!(req.session_id.as_deref(), Some("s1"));
        assert_eq!(req.agent.as_deref(), Some("explore"));
    }

    #[test]
    fn parses_simple_ops() {
        for (line, expected) in [
            (r#"{"id": "a", "op": "ping"}"#, Op::Ping),
            (r#"{"id": "a", "op": "interrupt"}"#, Op::Interrupt),
            (r#"{"id": "a", "op": "list_tools"}"#, Op::ListTools),
            (r#"{"id": "a", "op": "list_agents"}"#, Op::ListAgents),
            (r#"{"id": "a", "op": "list_sessions"}"#, Op::ListSessions),
            (r#"{"id": "a", "op": "info"}"#, Op::Info),
            (r#"{"id": "a", "op": "usage"}"#, Op::Usage),
        ] {
            let req = parse_request(line).unwrap();
            assert_eq!(req.op, expected, "failed for {}", line);
        }
    }

    #[test]
    fn parses_and_serializes_get_session() {
        let req = parse_request(r#"{"id": "a", "op": "get_session", "session_id": "s1"}"#).unwrap();
        assert_eq!(req.op, Op::GetSession);
        assert_eq!(req.session_id.as_deref(), Some("s1"));

        let line = ClientRequest::get_session("a", "s1").to_line();
        assert_eq!(line, r#"{"id":"a","op":"get_session","session_id":"s1"}"#);
    }

    #[test]
    fn get_session_requires_a_session_id() {
        let err = parse_request(r#"{"id": 1, "op": "get_session"}"#).unwrap_err();
        assert!(err.to_string().contains("session_id"), "{err}");
    }

    #[test]
    fn parses_and_serializes_delete_session() {
        let req =
            parse_request(r#"{"id": "a", "op": "delete_session", "session_id": "s1"}"#).unwrap();
        assert_eq!(req.op, Op::DeleteSession);
        assert_eq!(req.session_id.as_deref(), Some("s1"));

        let line = ClientRequest::delete_session("a", "s1").to_line();
        assert_eq!(
            line,
            r#"{"id":"a","op":"delete_session","session_id":"s1"}"#
        );
    }

    #[test]
    fn delete_session_requires_a_session_id() {
        let err = parse_request(r#"{"id": 1, "op": "delete_session"}"#).unwrap_err();
        assert!(err.to_string().contains("session_id"), "{err}");
    }

    #[test]
    fn deleted_reply_round_trip() {
        let deleted = ServerMessage::Deleted {
            id: Value::Number(1.into()),
            session_id: "s1".into(),
        };
        assert_eq!(deleted.kind(), "deleted");
        assert!(deleted.correlation_id().is_some());
        let decoded: ServerMessage =
            serde_json::from_str(&serde_json::to_string(&deleted).unwrap()).unwrap();
        assert_eq!(decoded, deleted);
    }

    #[test]
    fn info_request_and_reply_round_trip() {
        assert_eq!(
            ClientRequest::info("a").to_line(),
            r#"{"id":"a","op":"info"}"#
        );

        let info = ServerMessage::Info {
            id: Value::Number(1.into()),
            model: "gpt-4o".into(),
            provider: "openai".into(),
        };
        assert_eq!(info.kind(), "info");
        assert!(info.correlation_id().is_some());
        let decoded: ServerMessage =
            serde_json::from_str(&serde_json::to_string(&info).unwrap()).unwrap();
        assert_eq!(decoded, info);
    }

    #[test]
    fn usage_request_and_reply_round_trip() {
        assert_eq!(
            ClientRequest::usage("a").to_line(),
            r#"{"id":"a","op":"usage"}"#
        );
        assert_eq!(
            ClientRequest::usage("a").with_session("s1").to_line(),
            r#"{"id":"a","op":"usage","session_id":"s1"}"#
        );

        let usage = ServerMessage::Usage {
            id: Value::Number(1.into()),
            used_tokens: 34_000,
            context_limit: 128_000,
        };
        assert_eq!(usage.kind(), "usage");
        assert!(usage.correlation_id().is_some());
        let decoded: ServerMessage =
            serde_json::from_str(&serde_json::to_string(&usage).unwrap()).unwrap();
        assert_eq!(decoded, usage);
    }

    #[test]
    fn history_replies_round_trip() {
        let summary = SessionSummary {
            session_id: "s1".into(),
            message_count: 3,
            updated_at: "now".into(),
        };
        let sessions = ServerMessage::Sessions {
            id: Value::Number(1.into()),
            sessions: vec![summary],
        };
        assert_eq!(sessions.kind(), "sessions");
        assert!(sessions.correlation_id().is_some());
        assert_eq!(sessions.is_ask_terminal(), None);
        let decoded: ServerMessage =
            serde_json::from_str(&serde_json::to_string(&sessions).unwrap()).unwrap();
        assert_eq!(decoded, sessions);

        let session = ServerMessage::Session {
            id: Value::Number(2.into()),
            session_id: "s1".into(),
            messages: vec![Message::User {
                content: vec![ContentPart::Text { text: "hi".into() }],
            }],
        };
        assert_eq!(session.kind(), "session");
        assert!(session.correlation_id().is_some());
        let decoded: ServerMessage =
            serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();
        assert_eq!(decoded, session);
    }

    #[test]
    fn rejects_malformed_requests() {
        assert!(parse_request("not json").is_err());
        assert!(parse_request(r#"["array"]"#).is_err());
        assert!(parse_request(r#"{"op": "ping"}"#).is_err()); // missing id
        assert!(parse_request(r#"{"id": 1}"#).is_err()); // missing op
        assert!(parse_request(r#"{"id": 1, "op": "ask"}"#).is_err()); // missing prompt
        assert!(parse_request(r#"{"id": 1, "op": "nope"}"#).is_err()); // unknown op
    }

    #[test]
    fn error_messages_are_descriptive() {
        let err = parse_request(r#"{"op": "ping"}"#).unwrap_err();
        assert!(err.to_string().contains("id"));
        let err = parse_request(r#"{"id": 1, "op": "ask"}"#).unwrap_err();
        assert!(err.to_string().contains("prompt"));
    }

    #[test]
    fn constructors_produce_wire_format() {
        let line = ClientRequest::ask(7, "hello").to_line();
        assert_eq!(line, r#"{"id":7,"op":"ask","prompt":"hello"}"#);

        let line = ClientRequest::ask("1", "hi")
            .with_session("s1")
            .with_agent("explore")
            .to_line();
        assert_eq!(
            line,
            r#"{"id":"1","op":"ask","prompt":"hi","session_id":"s1","agent":"explore"}"#
        );

        let line = ClientRequest::ping("5").to_line();
        assert_eq!(line, r#"{"id":"5","op":"ping"}"#);

        let line = ClientRequest::interrupt(2).to_line();
        assert_eq!(line, r#"{"id":2,"op":"interrupt"}"#);

        let line = ClientRequest::list_tools("3").to_line();
        assert_eq!(line, r#"{"id":"3","op":"list_tools"}"#);

        let line = ClientRequest::list_agents("4").to_line();
        assert_eq!(line, r#"{"id":"4","op":"list_agents"}"#);
    }

    #[test]
    fn constructed_requests_round_trip_through_parser() {
        for req in [
            ClientRequest::ask(7, "hello"),
            ClientRequest::ask("1", "hi")
                .with_session("s1")
                .with_agent("explore"),
            ClientRequest::ping("5"),
            ClientRequest::interrupt(2),
            ClientRequest::list_tools("3"),
            ClientRequest::list_agents("4"),
        ] {
            let parsed = parse_request(&req.to_line()).unwrap();
            assert_eq!(parsed.id, req.id);
            assert_eq!(parsed.op, req.op);
            assert_eq!(parsed.session_id, req.session_id);
            assert_eq!(parsed.agent, req.agent);
        }
    }

    #[test]
    fn extract_id_works_on_malformed_messages() {
        assert_eq!(
            extract_id(r#"{"id": 9, "op": "nope"}"#),
            Some(Value::Number(9.into()))
        );
        assert_eq!(extract_id("not json"), None);
        assert_eq!(extract_id(r#"{"op": "ping"}"#), None);
        assert_eq!(extract_id(r#"[1,2]"#), None);
    }

    #[test]
    fn serializes_ready_message() {
        let msg = ServerMessage::Ready {
            id: Value::Number(1.into()),
            request_id: "req-1".to_string(),
            session_id: "sess-1".to_string(),
            agent: "build".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "ready");
        assert_eq!(json["id"], 1);
        assert_eq!(json["request_id"], "req-1");
        assert_eq!(json["session_id"], "sess-1");
        assert_eq!(json["agent"], "build");
    }

    #[test]
    fn serializes_event_message_with_agent_event() {
        let agent_event = AgentEvent::ContentDelta {
            delta: "hi".to_string(),
        };
        let msg = ServerMessage::Event {
            id: Value::Null,
            request_id: "req-1".to_string(),
            event: agent_event_to_value(&agent_event),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "event");
        assert_eq!(json["id"], Value::Null);
        assert_eq!(json["event"]["type"], "content_delta");
        assert_eq!(json["event"]["delta"], "hi");
    }

    #[test]
    fn event_messages_round_trip_through_deserialize() {
        let agent_event = AgentEvent::ToolStart {
            tool: "bash".to_string(),
            input: r#"{"command":"ls"}"#.to_string(),
            call_id: "call-1".to_string(),
        };
        let msg = ServerMessage::Event {
            id: Value::Null,
            request_id: "req-1".to_string(),
            event: agent_event_to_value(&agent_event),
        };
        let parsed: ServerMessage = serde_json::from_str(&msg.to_line()).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn serializes_tool_events() {
        let agent_event = AgentEvent::ToolStart {
            tool: "bash".to_string(),
            input: r#"{"command":"ls"}"#.to_string(),
            call_id: "call-1".to_string(),
        };
        let json = agent_event_to_value(&agent_event);
        assert_eq!(json["type"], "tool_start");
        assert_eq!(json["tool"], "bash");
    }

    #[test]
    fn serializes_error_message() {
        let msg = ServerMessage::Error {
            id: None,
            error: "boom".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["error"], "boom");
        assert!(json.get("id").is_none());

        // And it can be parsed back, including with an id present.
        let parsed: ServerMessage = serde_json::from_str(&msg.to_line()).unwrap();
        assert_eq!(parsed, msg);
        let with_id: ServerMessage =
            serde_json::from_str(r#"{"type":"error","id":7,"error":"boom"}"#).unwrap();
        assert_eq!(
            with_id,
            ServerMessage::Error {
                id: Some(Value::Number(7.into())),
                error: "boom".to_string()
            }
        );
    }

    #[test]
    fn serializes_interrupted_message() {
        let msg = ServerMessage::Interrupted {
            id: Some(Value::String("a".to_string())),
            request_id: Some("req-9".to_string()),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "interrupted");
        assert_eq!(json["id"], "a");
        assert_eq!(json["request_id"], "req-9");

        let parsed: ServerMessage = serde_json::from_str(&msg.to_line()).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn to_line_produces_single_line_json() {
        let msg = ServerMessage::Pong {
            id: Value::String("x".to_string()),
        };
        let line = msg.to_line();
        assert_eq!(line, r#"{"type":"pong","id":"x"}"#);
        assert!(!line.contains('\n'));

        let parsed: ServerMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn terminal_classification() {
        let ready = ServerMessage::Ready {
            id: Value::Null,
            request_id: "r".to_string(),
            session_id: "s".to_string(),
            agent: "build".to_string(),
        };
        assert_eq!(ready.is_ask_terminal(), None);

        let result = ServerMessage::Result {
            id: Value::Null,
            request_id: "r".to_string(),
            session_id: "s".to_string(),
            response: "done".to_string(),
        };
        assert_eq!(result.is_ask_terminal(), Some(true));

        let error = ServerMessage::Error {
            id: None,
            error: "x".to_string(),
        };
        assert_eq!(error.is_ask_terminal(), Some(false));
    }
}
