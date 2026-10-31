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
//! one); later requests reuse the session — a later request with a
//! *different* `session_id`/`agent` is rejected.
//!
//! When the server was started with a token (`gewu-server serve --token …`),
//! a connection must authenticate before any other operation:
//!
//! ```json
//! {"id": "0", "op": "auth", "token": "secret"}
//! ```
//!
//! # Server → Client
//!
//! ```json
//! {"type": "ready", "id": "1", "request_id": "c3f1...", "session_id": "s1", "agent": "build"}
//! {"type": "event", "id": "1", "request_id": "c3f1...", "event": {"type": "content_delta", "delta": "Hello"}}
//! {"type": "event", "id": "1", "request_id": "c3f1...", "event": {"type": "tool_start", "tool": "bash", "input": "{...}"}}
//! {"type": "result", "id": "1", "request_id": "c3f1...", "session_id": "s1", "response": "Done."}
//! {"type": "interrupted", "id": "1", "request_id": "c3f1..."}
//! {"type": "steered", "id": "7", "session_id": "s", "text": "focus on tests"}
//! {"type": "error", "id": "1", "error": "an ask is already in progress"}
//! {"type": "pong", "id": "5"}
//! {"type": "tools", "id": "3", "tools": ["read_file", "bash", "..."]}
//! {"type": "agents", "id": "4", "agents": ["build", "explore", "general"]}
//! {"type": "auth", "id": "0", "ok": true}
//! ```
//!
//! `event` payloads are serialized [`AgentEvent`]s (tagged with `"type"`:
//! `content_delta`, `reasoning_delta`, `tool_start`, `tool_progress`,
//! `tool_finish`, `usage`, `finish`, `error`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use gewu_core::question::{Question, QuestionOption};
pub use gewu_core::types::{
    AgentEvent, ContentPart, FunctionCall, Message, TokenUsage, ToolCall, summarize_tool_input,
};

/// Operations a client can request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    RevertChat {
        turn: Option<usize>,
        undo: bool,
    },
    /// Run an agent turn with the given prompt; events stream back.
    Ask {
        prompt: String,
    },
    /// Answer a pending `question` tool call. `answers` holds one label list
    /// per question, in the order the questions were asked.
    QuestionReply {
        request_id: String,
        answers: Vec<Vec<String>>,
    },
    /// Dismiss a pending `question` tool call without answering it.
    QuestionReject {
        request_id: String,
    },
    /// Attach to a session, restoring its live turn if one is running.
    SubscribeSession,
    /// Interrupt the currently running `ask` on this connection, if any.
    Interrupt,
    /// Inject a user message into the currently running `ask` (steering).
    /// The message joins the conversation at the next round boundary; it
    /// fails when no ask is running on the session.
    Steer {
        text: String,
    },
    /// List the tool names available to the agent.
    ListTools,
    /// Explicitly change the active profile without replacing the conversation.
    SwitchAgent {
        agent: String,
    },
    /// List visible files and directories directly within a working-directory path.
    ListFiles {
        path: String,
    },
    /// Read a bounded text file within the working directory.
    ReadFile {
        path: String,
    },
    /// Write a base64-encoded file into the working-directory root. The
    /// path must be a bare file name; existing files are overwritten.
    WriteFile {
        path: String,
        content_base64: String,
    },
    /// List the available agent profiles (build/explore/general).
    ListAgents,
    /// List the model selectors available for [`Op::SwitchModel`].
    ListModels,
    /// Switch the connection session's LLM model without replacing the
    /// conversation. `model` is a catalog selector (`endpoint/model`).
    SwitchModel {
        model: String,
    },
    /// Change the connection session's reasoning effort. `None` selects the
    /// model's configured default.
    SwitchReasoningEffort {
        effort: Option<String>,
    },
    /// Switch the session's active filesystem workspace root.
    SwitchWorkspace {
        workspace: String,
    },
    /// Browse direct child directories on the GeWu server host.
    BrowseWorkspace {
        path: String,
    },
    /// Add and select a server-host directory for the current session.
    AddWorkspace {
        path: String,
    },
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
    /// Authenticate the connection with the server's shared token. Required
    /// before any other operation when the server runs with `--token`.
    Auth {
        token: String,
    },
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
    /// Optional workspace id, honored only when the connection creates a new
    /// session. Existing sessions keep their persisted workspace.
    pub workspace: Option<String>,
}

/// Serialize in the protocol's wire format, e.g.
/// `{"id":1,"op":"ask","prompt":"hi","session_id":"s","agent":"build"}`.
impl Serialize for ClientRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let extra = self.session_id.is_some() as usize
            + if matches!(self.op, Op::RevertChat { .. }) {
                2
            } else {
                0
            }
            + (matches!(
                self.op,
                Op::SwitchAgent { .. }
                    | Op::SwitchModel { .. }
                    | Op::SwitchReasoningEffort { .. }
                    | Op::SwitchWorkspace { .. }
                    | Op::BrowseWorkspace { .. }
                    | Op::AddWorkspace { .. }
            )) as usize
            + self.agent.is_some() as usize
            + self.workspace.is_some() as usize
            + matches!(
                self.op,
                Op::Ask { .. }
                    | Op::Auth { .. }
                    | Op::Steer { .. }
                    | Op::ListFiles { .. }
                    | Op::ReadFile { .. }
                    | Op::WriteFile { .. }
            ) as usize
            + if matches!(
                self.op,
                Op::QuestionReply { .. } | Op::QuestionReject { .. }
            ) {
                2
            } else {
                0
            };
        let mut map = serializer.serialize_map(Some(2 + extra))?;
        map.serialize_entry("id", &self.id)?;
        match &self.op {
            Op::RevertChat { turn, undo } => {
                map.serialize_entry("op", "revert_chat")?;
                map.serialize_entry("turn", turn)?;
                map.serialize_entry("undo", undo)?;
            }
            Op::SwitchAgent { agent } => {
                map.serialize_entry("op", "switch_agent")?;
                map.serialize_entry("target_agent", agent)?;
            }
            Op::SwitchModel { model } => {
                map.serialize_entry("op", "switch_model")?;
                map.serialize_entry("model", model)?;
            }
            Op::SwitchReasoningEffort { effort } => {
                map.serialize_entry("op", "switch_reasoning_effort")?;
                map.serialize_entry("effort", effort)?;
            }
            Op::SwitchWorkspace { workspace } => {
                map.serialize_entry("op", "switch_workspace")?;
                map.serialize_entry("workspace", workspace)?;
            }
            Op::BrowseWorkspace { path } => {
                map.serialize_entry("op", "browse_workspace")?;
                map.serialize_entry("path", path)?;
            }
            Op::AddWorkspace { path } => {
                map.serialize_entry("op", "add_workspace")?;
                map.serialize_entry("path", path)?;
            }
            Op::Ask { prompt } => {
                map.serialize_entry("op", "ask")?;
                map.serialize_entry("prompt", prompt)?;
            }
            Op::QuestionReply {
                request_id,
                answers,
            } => {
                map.serialize_entry("op", "question_reply")?;
                map.serialize_entry("request_id", request_id)?;
                map.serialize_entry("answers", answers)?;
            }
            Op::QuestionReject { request_id } => {
                map.serialize_entry("op", "question_reject")?;
                map.serialize_entry("request_id", request_id)?;
            }
            Op::Auth { token } => {
                map.serialize_entry("op", "auth")?;
                map.serialize_entry("token", token)?;
            }
            Op::ListFiles { path } => {
                map.serialize_entry("op", "list_files")?;
                map.serialize_entry("path", path)?;
            }
            Op::ReadFile { path } => {
                map.serialize_entry("op", "read_file")?;
                map.serialize_entry("path", path)?;
            }
            Op::WriteFile {
                path,
                content_base64,
            } => {
                map.serialize_entry("op", "write_file")?;
                map.serialize_entry("path", path)?;
                map.serialize_entry("content_base64", content_base64)?;
            }
            Op::Interrupt => map.serialize_entry("op", "interrupt")?,
            Op::Steer { text } => {
                map.serialize_entry("op", "steer")?;
                map.serialize_entry("text", text)?;
            }
            Op::SubscribeSession => map.serialize_entry("op", "subscribe_session")?,
            Op::ListTools => map.serialize_entry("op", "list_tools")?,
            Op::ListAgents => map.serialize_entry("op", "list_agents")?,
            Op::ListModels => map.serialize_entry("op", "list_models")?,
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
        if let Some(workspace) = &self.workspace {
            map.serialize_entry("workspace", workspace)?;
        }
        map.end()
    }
}

impl ClientRequest {
    pub fn revert_chat(id: impl Into<Value>, turn: Option<usize>, undo: bool) -> Self {
        Self::simple(id, Op::RevertChat { turn, undo })
    }
    pub fn switch_agent(id: impl Into<Value>, agent: impl Into<String>) -> Self {
        Self::simple(
            id,
            Op::SwitchAgent {
                agent: agent.into(),
            },
        )
    }
    /// Build a `switch_model` request (a configured `endpoint/model` selector).
    pub fn switch_model(id: impl Into<Value>, model: impl Into<String>) -> Self {
        Self::simple(
            id,
            Op::SwitchModel {
                model: model.into(),
            },
        )
    }
    /// Build a `switch_reasoning_effort` request. `None` selects the model's
    /// configured default effort.
    pub fn switch_reasoning_effort(id: impl Into<Value>, effort: Option<String>) -> Self {
        Self::simple(id, Op::SwitchReasoningEffort { effort })
    }
    pub fn switch_workspace(id: impl Into<Value>, workspace: impl Into<String>) -> Self {
        Self::simple(
            id,
            Op::SwitchWorkspace {
                workspace: workspace.into(),
            },
        )
    }
    pub fn browse_workspace(id: impl Into<Value>, path: impl Into<String>) -> Self {
        Self::simple(id, Op::BrowseWorkspace { path: path.into() })
    }
    pub fn add_workspace(id: impl Into<Value>, path: impl Into<String>) -> Self {
        Self::simple(id, Op::AddWorkspace { path: path.into() })
    }
    /// Build an `ask` request.
    pub fn ask(id: impl Into<Value>, prompt: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            op: Op::Ask {
                prompt: prompt.into(),
            },
            session_id: None,
            agent: None,
            workspace: None,
        }
    }

    /// Build a `question_reply` request. `answers` holds one label list per
    /// question, in ask order; an empty list means "Unanswered".
    pub fn question_reply(
        id: impl Into<Value>,
        request_id: impl Into<String>,
        answers: Vec<Vec<String>>,
    ) -> Self {
        Self::simple(
            id,
            Op::QuestionReply {
                request_id: request_id.into(),
                answers,
            },
        )
    }

    /// Build a `question_reject` request (dismiss the pending form).
    pub fn question_reject(id: impl Into<Value>, request_id: impl Into<String>) -> Self {
        Self::simple(
            id,
            Op::QuestionReject {
                request_id: request_id.into(),
            },
        )
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

    /// Attach a workspace id for creation of a new session.
    pub fn with_workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    /// Build an `interrupt` request.
    pub fn interrupt(id: impl Into<Value>) -> Self {
        Self::simple(id, Op::Interrupt)
    }

    /// Build a `steer` request: inject `text` into the running ask.
    pub fn steer(id: impl Into<Value>, text: impl Into<String>) -> Self {
        Self::simple(id, Op::Steer { text: text.into() })
    }

    pub fn subscribe_session(id: impl Into<Value>, session_id: impl Into<String>) -> Self {
        Self::simple(id, Op::SubscribeSession).with_session(session_id)
    }

    /// Build a `list_tools` request.
    pub fn list_tools(id: impl Into<Value>) -> Self {
        Self::simple(id, Op::ListTools)
    }

    pub fn list_files(id: impl Into<Value>, path: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            op: Op::ListFiles { path: path.into() },
            session_id: None,
            agent: None,
            workspace: None,
        }
    }

    pub fn read_file(id: impl Into<Value>, path: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            op: Op::ReadFile { path: path.into() },
            session_id: None,
            agent: None,
            workspace: None,
        }
    }

    /// Build a `write_file` request: `content_base64` is the base64-encoded
    /// file body; `path` must be a bare file name inside the working
    /// directory root.
    pub fn write_file(
        id: impl Into<Value>,
        path: impl Into<String>,
        content_base64: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            op: Op::WriteFile {
                path: path.into(),
                content_base64: content_base64.into(),
            },
            session_id: None,
            agent: None,
            workspace: None,
        }
    }

    /// Build a `list_agents` request.
    pub fn list_agents(id: impl Into<Value>) -> Self {
        Self::simple(id, Op::ListAgents)
    }

    /// Build a `list_models` request (the switchable model catalog).
    pub fn list_models(id: impl Into<Value>) -> Self {
        Self::simple(id, Op::ListModels)
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

    /// Build an `auth` request (required before other ops on token-protected
    /// servers).
    pub fn auth(id: impl Into<Value>, token: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            op: Op::Auth {
                token: token.into(),
            },
            session_id: None,
            agent: None,
            workspace: None,
        }
    }

    fn simple(id: impl Into<Value>, op: Op) -> Self {
        Self {
            id: id.into(),
            op,
            session_id: None,
            agent: None,
            workspace: None,
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

/// Parse a `[["label", ...], ...]` answer payload; `None` when malformed.
fn parse_answers(value: Option<&Value>) -> Option<Vec<Vec<String>>> {
    let outer = value?.as_array()?;
    let mut answers = Vec::with_capacity(outer.len());
    for answer in outer {
        let labels = answer.as_array()?;
        let mut parsed = Vec::with_capacity(labels.len());
        for label in labels {
            parsed.push(label.as_str()?.to_string());
        }
        answers.push(parsed);
    }
    Some(answers)
}

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
        "revert_chat" => {
            let turn = match obj.get("turn") {
                None | Some(Value::Null) => None,
                Some(value) => Some(
                    value
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or_else(|| {
                            RequestError::Invalid("turn must be a nonnegative integer".into())
                        })?,
                ),
            };
            let undo = match obj.get("undo") {
                None => false,
                Some(value) => value
                    .as_bool()
                    .ok_or_else(|| RequestError::Invalid("undo must be boolean".into()))?,
            };
            Op::RevertChat { turn, undo }
        }
        "switch_agent" => Op::SwitchAgent {
            agent: obj
                .get("target_agent")
                .and_then(Value::as_str)
                .ok_or_else(|| RequestError::Invalid("switch_agent requires target_agent".into()))?
                .to_string(),
        },
        "switch_model" => Op::SwitchModel {
            model: obj
                .get("model")
                .and_then(Value::as_str)
                .ok_or_else(|| RequestError::Invalid("switch_model requires model".into()))?
                .to_string(),
        },
        "switch_reasoning_effort" => {
            let effort = match obj.get("effort") {
                None | Some(Value::Null) => None,
                Some(Value::String(effort)) => Some(effort.clone()),
                Some(_) => {
                    return Err(RequestError::Invalid(
                        "effort must be a string or null".into(),
                    ));
                }
            };
            Op::SwitchReasoningEffort { effort }
        }
        "switch_workspace" => Op::SwitchWorkspace {
            workspace: obj
                .get("workspace")
                .and_then(Value::as_str)
                .ok_or_else(|| RequestError::Invalid("switch_workspace requires workspace".into()))?
                .to_string(),
        },
        "browse_workspace" => Op::BrowseWorkspace {
            path: obj
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| RequestError::Invalid("browse_workspace requires path".into()))?
                .to_string(),
        },
        "add_workspace" => Op::AddWorkspace {
            path: obj
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| RequestError::Invalid("add_workspace requires path".into()))?
                .to_string(),
        },
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
        "question_reply" => {
            let request_id = obj
                .get("request_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    RequestError::Invalid(
                        "op 'question_reply' requires a 'request_id' string".to_string(),
                    )
                })?
                .to_string();
            let answers = parse_answers(obj.get("answers")).ok_or_else(|| {
                RequestError::Invalid(
                    "op 'question_reply' requires 'answers' (an array of string arrays)"
                        .to_string(),
                )
            })?;
            Op::QuestionReply {
                request_id,
                answers,
            }
        }
        "question_reject" => Op::QuestionReject {
            request_id: obj
                .get("request_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    RequestError::Invalid(
                        "op 'question_reject' requires a 'request_id' string".to_string(),
                    )
                })?
                .to_string(),
        },
        "interrupt" => Op::Interrupt,
        "steer" => Op::Steer {
            text: obj
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    RequestError::Invalid("op 'steer' requires a 'text' string".to_string())
                })?
                .to_string(),
        },
        "list_tools" => Op::ListTools,
        "list_files" | "read_file" => {
            let path = obj
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    RequestError::Invalid(format!("op '{}' requires a 'path' string", op_str))
                })?
                .to_string();
            if op_str == "list_files" {
                Op::ListFiles { path }
            } else {
                Op::ReadFile { path }
            }
        }
        "list_agents" => Op::ListAgents,
        "list_models" => Op::ListModels,
        "write_file" => {
            let path = obj
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    RequestError::Invalid("op 'write_file' requires a 'path' string".to_string())
                })?
                .to_string();
            let content_base64 = obj
                .get("content_base64")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    RequestError::Invalid(
                        "op 'write_file' requires a 'content_base64' string".to_string(),
                    )
                })?
                .to_string();
            Op::WriteFile {
                path,
                content_base64,
            }
        }
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
        "subscribe_session" => Op::SubscribeSession,
        "ping" => Op::Ping,
        "info" => Op::Info,
        "usage" => Op::Usage,
        "auth" => {
            let token = obj
                .get("token")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    RequestError::Invalid("op 'auth' requires a 'token' string".to_string())
                })?
                .to_string();
            Op::Auth { token }
        }
        other => {
            return Err(RequestError::Invalid(format!("unknown op '{}'", other)));
        }
    };

    let session_id = obj
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let agent = obj.get("agent").and_then(Value::as_str).map(str::to_string);
    let workspace = obj
        .get("workspace")
        .and_then(Value::as_str)
        .map(str::to_string);

    Ok(ClientRequest {
        id,
        op,
        session_id,
        agent,
        workspace,
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
    #[serde(default = "default_agent_name")]
    pub agent: String,
    /// Server session id, usable to resume the conversation.
    pub session_id: String,
    /// Number of stored messages.
    pub message_count: i64,
    /// Human-readable time of the most recent message.
    pub updated_at: String,
}

fn default_agent_name() -> String {
    "build".into()
}

fn default_workspace_id() -> String {
    "cwd".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
}

/// One configured or runtime filesystem root visible to clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceSummary {
    pub id: String,
    pub path: String,
}

/// One direct child directory on the GeWu server host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
}

/// One switchable model in the server's catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSummary {
    /// `endpoint/model` selector accepted by `switch_model`.
    pub selector: String,
    /// Human-readable model id shown in UIs.
    pub label: String,
    /// Whether the model accepts runtime reasoning-effort changes.
    #[serde(default)]
    pub supports_reasoning_effort: bool,
    /// The effort the model runs with when no explicit effort is selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

/// Reasoning-effort values exposed by clients. Model support is reported by
/// [`ModelSummary::supports_reasoning_effort`]; the model's configured default
/// remains selectable separately.
pub const REASONING_EFFORTS: &[&str] = &["low", "medium", "high", "max"];

/// Live progress of one workflow node, as shown in a run's graph.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowNodeState {
    pub id: String,
    /// `ok`, `failed`, `pending`, or `skipped`.
    pub status: String,
    pub done: u32,
    pub failed: u32,
}

/// Live state of a background workflow run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowRunState {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `running`, `completed`, `canceled`, `timed out`, or `failed`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_node: Option<String>,
    pub tasks_done: u32,
    pub tasks_failed: u32,
    pub tasks_attempted: u32,
    pub nodes: Vec<WorkflowNodeState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Messages the server sends to the client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Consistent reconnect boundary: history before the turn and every
    /// streamed frame through `seq`. Subsequent frames are newer than it.
    SessionSnapshot {
        session_id: String,
        messages: Vec<Message>,
        frames: Vec<ServerMessage>,
        busy: bool,
        seq: u64,
        /// The session's model selector (`endpoint/model`), when the sending
        /// server tracks it. `None`/absent from older servers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// The session's explicit reasoning effort; absent means the model's
        /// configured default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<String>,
    },
    AgentSwitched {
        id: Value,
        session_id: String,
        agent: String,
        tools: Vec<String>,
    },
    /// Reply to a `switch_model` request: the session now runs the model.
    ModelSwitched {
        /// Echo of the client request id.
        id: Value,
        /// Session the switch applied to.
        session_id: String,
        /// The session's new model selector.
        model: String,
        /// The new model's display label.
        label: String,
        /// The session's explicit reasoning effort after the switch; absent
        /// means the new model's configured default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<String>,
    },
    /// Reply to a `switch_reasoning_effort` request.
    ReasoningEffortSwitched {
        /// Echo of the client request id.
        id: Value,
        /// Session the switch applied to.
        session_id: String,
        /// The session's explicit effort; `None` means the model default.
        effort: Option<String>,
    },
    /// Reply after changing the session's active filesystem root.
    WorkspaceSwitched {
        id: Value,
        session_id: String,
        workspace: WorkspaceSummary,
    },
    /// Direct child directories on the GeWu server host.
    Directories {
        id: Value,
        path: String,
        parent: Option<String>,
        entries: Vec<DirectoryEntry>,
    },
    /// A runtime directory was added and selected for a session.
    WorkspaceAdded {
        id: Value,
        session_id: String,
        workspace: WorkspaceSummary,
    },
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
    /// Live progress of a background workflow run, pushed to subscribers of
    /// the session that started it. Unsolicited: it carries no request id.
    WorkflowProgress {
        /// Session that owns the run.
        session_id: String,
        /// The run's current state.
        run: WorkflowRunState,
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
    /// Reply to a `steer` request: the message was queued into the
    /// running ask. The steered message itself also arrives as a `steered`
    /// agent event on the ask's stream.
    Steered {
        /// Echo of the client request id.
        id: Value,
        /// Session the message was injected into.
        session_id: String,
        /// The injected text.
        text: String,
    },
    /// Reply to a `question_reply`/`question_reject` request.
    QuestionReplied {
        /// Echo of the client request id.
        id: Value,
        /// Session that owns the question.
        session_id: String,
        /// The question request that was settled.
        request_id: String,
        /// Accepted answers; `None` means the user dismissed the question.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answers: Option<Vec<Vec<String>>>,
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
        /// The working directory the server's agent operates in (absolute).
        #[serde(default)]
        working_directory: String,
        /// All configured roots; empty when talking to an older server.
        #[serde(default)]
        workspaces: Vec<WorkspaceSummary>,
        /// The active root for the requested session, or `cwd` when no
        /// session has been attached.
        #[serde(default = "default_workspace_id")]
        active_workspace: String,
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
        /// Whether an ask is currently running on this connection. When
        /// `true`, `used_tokens`/`context_limit` are `0` because the agent
        /// lock is held by the running ask and cannot be measured; `0/0`
        /// with `busy: false` instead means no session exists yet.
        #[serde(default)]
        busy: bool,
        /// Provider-reported token accounting accumulated across the
        /// session (cache hits/writes, uncached input, output). `None` when
        /// the provider has not reported any usage yet; defaults to `None`
        /// when decoding frames from older servers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_usage: Option<TokenUsage>,
    },
    /// Reply to an `auth` request. When `ok` is `false` the server closes
    /// the connection right after this frame.
    Auth {
        /// Echo of the client request id.
        id: Value,
        /// Whether the token was accepted.
        ok: bool,
    },
    /// Reply to a `list_tools` request.
    Tools {
        /// Echo of the client request id.
        id: Value,
        /// Names of the tools available to the agent.
        tools: Vec<String>,
    },
    /// One level of visible working-directory entries.
    Files {
        id: Value,
        path: String,
        entries: Vec<FileEntry>,
    },
    /// Bounded text file contents for the GUI preview.
    FileContent {
        id: Value,
        path: String,
        content: String,
        truncated: bool,
    },
    /// Reply to a `write_file` request: the file was created or overwritten.
    FileWritten {
        /// Echo of the client request id.
        id: Value,
        /// The file name the content was written under.
        path: String,
        /// Number of bytes written.
        bytes: u64,
    },
    /// Reply to a `list_agents` request.
    Agents {
        /// Echo of the client request id.
        id: Value,
        /// Available agent profile names.
        agents: Vec<String>,
    },
    /// Reply to a `list_models` request.
    Models {
        /// Echo of the client request id.
        id: Value,
        /// Switchable models, sorted by selector.
        models: Vec<ModelSummary>,
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
            ServerMessage::SessionSnapshot { .. } => "session_snapshot",
            ServerMessage::AgentSwitched { .. } => "agent_switched",
            ServerMessage::ModelSwitched { .. } => "model_switched",
            ServerMessage::ReasoningEffortSwitched { .. } => "reasoning_effort_switched",
            ServerMessage::WorkspaceSwitched { .. } => "workspace_switched",
            ServerMessage::Directories { .. } => "directories",
            ServerMessage::WorkspaceAdded { .. } => "workspace_added",
            ServerMessage::Ready { .. } => "ready",
            ServerMessage::Event { .. } => "event",
            ServerMessage::WorkflowProgress { .. } => "workflow_progress",
            ServerMessage::Result { .. } => "result",
            ServerMessage::Interrupted { .. } => "interrupted",
            ServerMessage::Steered { .. } => "steered",
            ServerMessage::QuestionReplied { .. } => "question_replied",
            ServerMessage::Error { .. } => "error",
            ServerMessage::Pong { .. } => "pong",
            ServerMessage::Info { .. } => "info",
            ServerMessage::Usage { .. } => "usage",
            ServerMessage::Auth { .. } => "auth",
            ServerMessage::Tools { .. } => "tools",
            ServerMessage::Files { .. } => "files",
            ServerMessage::FileContent { .. } => "file_content",
            ServerMessage::FileWritten { .. } => "file_written",
            ServerMessage::Agents { .. } => "agents",
            ServerMessage::Models { .. } => "models",
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
            ServerMessage::AgentSwitched { id, .. } => Some(id),
            ServerMessage::ModelSwitched { id, .. } => Some(id),
            ServerMessage::ReasoningEffortSwitched { id, .. } => Some(id),
            ServerMessage::WorkspaceSwitched { id, .. } => Some(id),
            ServerMessage::Directories { id, .. } => Some(id),
            ServerMessage::WorkspaceAdded { id, .. } => Some(id),
            ServerMessage::Ready { id, .. }
            | ServerMessage::Pong { id }
            | ServerMessage::Info { id, .. }
            | ServerMessage::Usage { id, .. }
            | ServerMessage::Auth { id, .. }
            | ServerMessage::Tools { id, .. }
            | ServerMessage::Files { id, .. }
            | ServerMessage::FileContent { id, .. }
            | ServerMessage::FileWritten { id, .. }
            | ServerMessage::Agents { id, .. }
            | ServerMessage::Models { id, .. }
            | ServerMessage::Sessions { id, .. }
            | ServerMessage::Session { id, .. }
            | ServerMessage::Deleted { id, .. } => Some(id),
            ServerMessage::Interrupted { id, .. } | ServerMessage::Error { id, .. } => id.as_ref(),
            ServerMessage::Steered { id, .. } => Some(id),
            ServerMessage::QuestionReplied { id, .. } => Some(id),
            ServerMessage::Event { .. }
            | ServerMessage::Result { .. }
            | ServerMessage::SessionSnapshot { .. }
            | ServerMessage::WorkflowProgress { .. } => None,
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
    fn chat_revert_requests_round_trip() {
        for (turn, undo) in [(None, false), (Some(2), false), (None, true)] {
            let request = ClientRequest::revert_chat(1, turn, undo);
            assert_eq!(parse_request(&request.to_line()).unwrap(), request);
        }
        assert!(parse_request(r#"{"id":1,"op":"revert_chat","turn":-1}"#).is_err());
    }

    #[test]
    fn question_requests_and_reply_round_trip() {
        let answers = vec![vec!["Postgres".to_string()], vec![]];
        let request = ClientRequest::question_reply(4, "que_1", answers.clone()).with_session("s");
        assert_eq!(parse_request(&request.to_line()).unwrap(), request);
        assert_eq!(
            request.to_line(),
            r#"{"id":4,"op":"question_reply","request_id":"que_1","answers":[["Postgres"],[]],"session_id":"s"}"#
        );

        let reject = ClientRequest::question_reject(5, "que_1");
        assert_eq!(parse_request(&reject.to_line()).unwrap(), reject);
        assert!(parse_request(r#"{"id":1,"op":"question_reply"}"#).is_err());
        assert!(parse_request(r#"{"id":1,"op":"question_reject"}"#).is_err());
        assert!(
            parse_request(r#"{"id":1,"op":"question_reply","request_id":"q","answers":[1]}"#)
                .is_err()
        );

        let reply = ServerMessage::QuestionReplied {
            id: 4.into(),
            session_id: "s".into(),
            request_id: "que_1".into(),
            answers: Some(answers),
        };
        let decoded: ServerMessage = serde_json::from_str(&reply.to_line()).unwrap();
        assert_eq!(decoded, reply);
        assert_eq!(reply.kind(), "question_replied");
        assert_eq!(reply.correlation_id(), Some(&4.into()));

        let dismissed = ServerMessage::QuestionReplied {
            id: 5.into(),
            session_id: "s".into(),
            request_id: "que_1".into(),
            answers: None,
        };
        let decoded: ServerMessage = serde_json::from_str(&dismissed.to_line()).unwrap();
        assert_eq!(decoded, dismissed);
    }

    #[test]
    fn steer_request_and_reply_round_trip() {
        let request = ClientRequest::steer(9, "focus on tests").with_session("s");
        assert_eq!(parse_request(&request.to_line()).unwrap(), request);
        assert!(parse_request(r#"{"id":1,"op":"steer"}"#).is_err());

        let reply = ServerMessage::Steered {
            id: 9.into(),
            session_id: "s".into(),
            text: "focus on tests".into(),
        };
        let decoded: ServerMessage = serde_json::from_str(&reply.to_line()).unwrap();
        assert_eq!(decoded, reply);
        assert_eq!(reply.kind(), "steered");
        assert_eq!(reply.correlation_id(), Some(&9.into()));
        // Steering never terminates the ask it injects into.
        assert_eq!(reply.is_ask_terminal(), None);
    }

    #[test]
    fn workflow_progress_round_trip() {
        let progress = ServerMessage::WorkflowProgress {
            session_id: "s".into(),
            run: WorkflowRunState {
                run_id: "r-1".into(),
                name: Some("docs".into()),
                status: "running".into(),
                current_node: Some("read".into()),
                tasks_done: 1,
                tasks_failed: 0,
                tasks_attempted: 1,
                nodes: vec![
                    WorkflowNodeState {
                        id: "read".into(),
                        status: "ok".into(),
                        done: 1,
                        failed: 0,
                    },
                    WorkflowNodeState {
                        id: "write".into(),
                        status: "pending".into(),
                        done: 0,
                        failed: 0,
                    },
                ],
                error: None,
            },
        };
        assert_eq!(progress.kind(), "workflow_progress");
        assert_eq!(progress.correlation_id(), None);
        assert_eq!(progress.is_ask_terminal(), None);
        let decoded: ServerMessage = serde_json::from_str(&progress.to_line()).unwrap();
        assert_eq!(decoded, progress);
    }

    #[test]
    fn switch_agent_round_trip() {
        let request = ClientRequest::switch_agent(7, "plan");
        assert_eq!(parse_request(&request.to_line()).unwrap(), request);
        assert!(parse_request(r#"{"id":1,"op":"switch_agent"}"#).is_err());
        let reply = ServerMessage::AgentSwitched {
            id: 7.into(),
            session_id: "s".into(),
            agent: "plan".into(),
            tools: vec![],
        };
        assert_eq!(reply.kind(), "agent_switched");
        assert_eq!(reply.correlation_id(), Some(&Value::from(7)));
        assert_eq!(
            serde_json::from_str::<ServerMessage>(&reply.to_line()).unwrap(),
            reply
        );
    }

    #[test]
    fn switch_model_round_trip() {
        let request = ClientRequest::switch_model(7, "west/coding");
        assert_eq!(parse_request(&request.to_line()).unwrap(), request);
        assert_eq!(
            request.to_line(),
            r#"{"id":7,"op":"switch_model","model":"west/coding"}"#
        );
        assert!(parse_request(r#"{"id":1,"op":"switch_model"}"#).is_err());
        let reply = ServerMessage::ModelSwitched {
            id: 7.into(),
            session_id: "s".into(),
            model: "west/coding".into(),
            label: "deepseek-reasoner".into(),
            reasoning_effort: None,
        };
        assert_eq!(reply.kind(), "model_switched");
        assert_eq!(reply.correlation_id(), Some(&Value::from(7)));
        assert_eq!(reply.is_ask_terminal(), None);
        assert_eq!(
            serde_json::from_str::<ServerMessage>(&reply.to_line()).unwrap(),
            reply
        );
    }

    #[test]
    fn list_models_round_trip() {
        let request = ClientRequest::list_models(3);
        assert_eq!(request.to_line(), r#"{"id":3,"op":"list_models"}"#);
        assert_eq!(parse_request(&request.to_line()).unwrap(), request);
        let reply = ServerMessage::Models {
            id: 3.into(),
            models: vec![ModelSummary {
                selector: "west/coding".into(),
                label: "deepseek-reasoner".into(),
                supports_reasoning_effort: true,
                default_effort: Some("high".into()),
            }],
        };
        assert_eq!(reply.kind(), "models");
        assert_eq!(reply.correlation_id(), Some(&Value::from(3)));
        let decoded: ServerMessage =
            serde_json::from_str(&serde_json::to_string(&reply).unwrap()).unwrap();
        assert_eq!(decoded, reply);
    }

    #[test]
    fn session_snapshot_model_field_is_optional() {
        let legacy = r#"{"type":"session_snapshot","session_id":"s1","messages":[],"frames":[],"busy":false,"seq":0}"#;
        let decoded: ServerMessage = serde_json::from_str(legacy).unwrap();
        match decoded {
            ServerMessage::SessionSnapshot { model, .. } => assert_eq!(model, None),
            other => panic!("expected session_snapshot, got {}", other.kind()),
        }
        let with_model = ServerMessage::SessionSnapshot {
            session_id: "s1".into(),
            messages: vec![],
            frames: vec![],
            busy: false,
            seq: 0,
            model: Some("west/coding".into()),
            reasoning_effort: Some("high".into()),
        };
        let decoded: ServerMessage =
            serde_json::from_str(&serde_json::to_string(&with_model).unwrap()).unwrap();
        assert_eq!(decoded, with_model);
    }

    #[test]
    fn switch_reasoning_effort_round_trip() {
        for effort in [Some("high".to_string()), None] {
            let request = ClientRequest::switch_reasoning_effort(7, effort.clone());
            assert_eq!(parse_request(&request.to_line()).unwrap(), request);
            assert_eq!(request.op, Op::SwitchReasoningEffort { effort });
        }
        assert!(parse_request(r#"{"id":1,"op":"switch_reasoning_effort","effort":4}"#).is_err());
        let reply = ServerMessage::ReasoningEffortSwitched {
            id: 7.into(),
            session_id: "s".into(),
            effort: Some("high".into()),
        };
        assert_eq!(reply.kind(), "reasoning_effort_switched");
        assert_eq!(reply.correlation_id(), Some(&Value::from(7)));
        assert_eq!(
            serde_json::from_str::<ServerMessage>(&reply.to_line()).unwrap(),
            reply
        );
    }

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
            working_directory: "/tmp/proj".into(),
            workspaces: vec![],
            active_workspace: "cwd".into(),
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
            busy: false,
            provider_usage: Some(TokenUsage {
                input_tokens: 1_000,
                cached_tokens: 30_000,
                cache_write_tokens: 3_000,
                output_tokens: 500,
                reasoning_tokens: 120,
            }),
        };
        assert_eq!(usage.kind(), "usage");
        assert!(usage.correlation_id().is_some());
        let encoded = serde_json::to_string(&usage).unwrap();
        assert!(encoded.contains("provider_usage"), "{encoded}");
        let decoded: ServerMessage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, usage);

        // `busy`/`provider_usage` default when decoding frames from older
        // servers.
        let legacy: ServerMessage =
            serde_json::from_str(r#"{"type":"usage","id":1,"used_tokens":5,"context_limit":6}"#)
                .unwrap();
        match legacy {
            ServerMessage::Usage {
                busy,
                provider_usage,
                ..
            } => {
                assert!(!busy);
                assert!(provider_usage.is_none());
            }
            other => panic!("expected usage, got {}", other.kind()),
        }
    }

    #[test]
    fn auth_round_trips() {
        let req = parse_request(r#"{"id": 0, "op": "auth", "token": "secret"}"#).unwrap();
        assert_eq!(
            req.op,
            Op::Auth {
                token: "secret".into()
            }
        );
        assert_eq!(
            ClientRequest::auth(0, "secret").to_line(),
            r#"{"id":0,"op":"auth","token":"secret"}"#
        );

        let err = parse_request(r#"{"id": 1, "op": "auth"}"#).unwrap_err();
        assert!(err.to_string().contains("token"), "{err}");

        let ok = ServerMessage::Auth {
            id: Value::Number(0.into()),
            ok: true,
        };
        assert_eq!(ok.kind(), "auth");
        assert!(ok.correlation_id().is_some());
        assert_eq!(ok.is_ask_terminal(), None);
        let decoded: ServerMessage =
            serde_json::from_str(&serde_json::to_string(&ok).unwrap()).unwrap();
        assert_eq!(decoded, ok);
    }

    #[test]
    fn history_replies_round_trip() {
        let summary = SessionSummary {
            agent: "build".into(),
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

        let line = ClientRequest::write_file("9", "data.bin", "aGk=").to_line();
        assert_eq!(
            line,
            r#"{"id":"9","op":"write_file","path":"data.bin","content_base64":"aGk="}"#
        );
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
            ClientRequest::write_file("9", "data.bin", "aGk="),
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
    fn status_event_serializes_and_round_trips() {
        let agent_event = AgentEvent::Status {
            message: "Compacting context…".to_string(),
        };
        let value = agent_event_to_value(&agent_event);
        assert_eq!(value["type"], "status");
        assert_eq!(value["message"], "Compacting context…");

        let decoded: AgentEvent = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, agent_event);
    }

    #[test]
    fn retrying_event_serializes_and_round_trips() {
        let agent_event = AgentEvent::Retrying {
            kind: gewu_core::types::RetryKind::Status(502),
            message: "API error: 502 Bad Gateway: <html>upstream exploded</html>".to_string(),
            attempt: 2,
            delay_secs: 4,
        };
        let value = agent_event_to_value(&agent_event);
        assert_eq!(value["type"], "retrying");
        // The kind serializes as a flat string tag, stable across volatile
        // bodies; the message carries the human-readable detail.
        assert_eq!(value["kind"], "status-502");
        assert_eq!(
            value["message"],
            "API error: 502 Bad Gateway: <html>upstream exploded</html>"
        );
        assert_eq!(value["attempt"], 2);
        assert_eq!(value["delay_secs"], 4);

        let decoded: AgentEvent = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, agent_event);
        assert_eq!(
            serde_json::to_value(AgentEvent::Retrying {
                kind: gewu_core::types::RetryKind::RateLimit,
                message: "slow down".into(),
                attempt: 1,
                delay_secs: 2,
            })
            .unwrap()["kind"],
            "rate-limit"
        );
        // One display source for every frontend: the prefix + live tail.
        assert_eq!(
            AgentEvent::retry_prefix("boom"),
            "LLM request failed: boom; retrying in"
        );
        assert_eq!(
            AgentEvent::retry_message("boom", 2, 4),
            "LLM request failed: boom; retrying in 4s (attempt 2)"
        );
    }

    #[test]
    fn event_messages_round_trip_through_deserialize() {
        let agent_event = AgentEvent::ToolStart {
            call_id: "call-1".to_string(),
            tool: "bash".to_string(),
            input: r#"{"command":"ls"}"#.to_string(),
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
            call_id: "call-1".to_string(),
            tool: "bash".to_string(),
            input: r#"{"command":"ls"}"#.to_string(),
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
    fn workspace_switch_request_round_trips() {
        let req = parse_request(
            r#"{"id":"a","op":"switch_workspace","workspace":"frontend","session_id":"s1"}"#,
        )
        .unwrap();
        assert_eq!(
            req.op,
            Op::SwitchWorkspace {
                workspace: "frontend".into()
            }
        );
        assert_eq!(
            ClientRequest::switch_workspace("a", "frontend")
                .with_session("s1")
                .to_line(),
            r#"{"id":"a","op":"switch_workspace","workspace":"frontend","session_id":"s1"}"#
        );
    }

    #[test]
    fn runtime_workspace_requests_round_trip() {
        let browse = parse_request(r#"{"id":"a","op":"browse_workspace","path":"/tmp"}"#).unwrap();
        assert_eq!(
            browse.op,
            Op::BrowseWorkspace {
                path: "/tmp".into()
            }
        );
        assert_eq!(
            ClientRequest::add_workspace("b", "/tmp/project")
                .with_session("s1")
                .to_line(),
            r#"{"id":"b","op":"add_workspace","path":"/tmp/project","session_id":"s1"}"#
        );
    }

    #[test]
    fn workspace_info_is_backward_compatible() {
        let legacy: ServerMessage = serde_json::from_str(
            r#"{"type":"info","id":1,"model":"m","provider":"p","working_directory":"/tmp"}"#,
        )
        .unwrap();
        match legacy {
            ServerMessage::Info {
                workspaces,
                active_workspace,
                ..
            } => {
                assert!(workspaces.is_empty());
                assert_eq!(active_workspace, "cwd");
            }
            other => panic!("expected info, got {}", other.kind()),
        }
    }

    #[test]
    fn creation_workspace_round_trips_on_first_ask() {
        let request = ClientRequest::ask("a", "pwd").with_workspace("tachyon");
        assert_eq!(
            request.to_line(),
            r#"{"id":"a","op":"ask","prompt":"pwd","workspace":"tachyon"}"#
        );
        let parsed = parse_request(&request.to_line()).unwrap();
        assert_eq!(parsed.workspace.as_deref(), Some("tachyon"));
    }

    #[test]
    fn workspace_messages_have_kinds_and_ids() {
        let switched = ServerMessage::WorkspaceSwitched {
            id: Value::from(4),
            session_id: "s".into(),
            workspace: WorkspaceSummary {
                id: "frontend".into(),
                path: "/work/frontend".into(),
            },
        };
        assert_eq!(switched.kind(), "workspace_switched");
        assert_eq!(switched.correlation_id(), Some(&Value::from(4)));

        let added = ServerMessage::WorkspaceAdded {
            id: Value::from(5),
            session_id: "s".into(),
            workspace: WorkspaceSummary {
                id: "runtime:/work/other".into(),
                path: "/work/other".into(),
            },
        };
        assert_eq!(added.kind(), "workspace_added");
        assert_eq!(added.correlation_id(), Some(&Value::from(5)));
        let decoded: ServerMessage =
            serde_json::from_str(&serde_json::to_string(&added).unwrap()).unwrap();
        assert_eq!(decoded, added);

        let dirs = ServerMessage::Directories {
            id: Value::from(6),
            path: "/work".into(),
            parent: Some("/".into()),
            entries: vec![DirectoryEntry {
                name: "project".into(),
                path: "/work/project".into(),
            }],
        };
        assert_eq!(dirs.kind(), "directories");
        assert_eq!(dirs.correlation_id(), Some(&Value::from(6)));
        let decoded: ServerMessage =
            serde_json::from_str(&serde_json::to_string(&dirs).unwrap()).unwrap();
        assert_eq!(decoded, dirs);
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
