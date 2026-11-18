//! Core type definitions for the GeWu agent framework.
//!
//! This module provides the fundamental data structures used throughout GeWu:
//! - [`Message`] - Conversation messages with different roles (system, user, assistant, tool)
//! - [`ContentPart`] - Content within messages (text, images, audio, video, files)
//! - [`FunctionCall`] / [`ToolCall`] - Tool invocation structures
//! - [`ProviderEvent`] - Events emitted by LLM providers during streaming
//! - [`AgentEvent`] - Events emitted by the agent during agent execution
//! - [`Capabilities`] - Model capabilities for input/output modalities
//! - [`ToolResult`] - Structured result from tool execution
//! - [`ToolContext`] - Execution context passed to tools
//! - [`PathEnforcement`] - File-path confinement level for tools
//! - [`SessionState`] - Typed handle for per-session tool state
//! - [`FileAttachment`] - File attachment in tool results
//! - [`TruncationResult`] - Result of output truncation

use crate::traits::SessionStateStore;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

/// Represents a function call within a tool call.
///
/// Contains the name of the function to invoke and its arguments as a JSON string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    /// The name of the function to call.
    pub name: String,
    /// The arguments to pass to the function, encoded as a JSON string.
    pub arguments: String,
}

/// Represents a tool call from the LLM.
///
/// Tool calls are how the agent requests execution of tools like bash, file operations, etc.
/// Each tool call has a unique ID (assigned by the LLM provider), a type (always "function"),
/// and the actual function to call with its arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// A unique identifier for this tool call, assigned by the LLM provider.
    /// This ID is used to match tool responses with tool call requests.
    pub id: String,
    /// The type of tool call. Currently always "function".
    pub r#type: String,
    /// The function to call and its arguments.
    pub function: FunctionCall,
}

/// Content part of a message, supporting various media types.
///
/// ContentPart represents the actual content within a message. It can be plain text,
/// or media types like images, audio, video, or generic files. Media types include
/// both the data (typically base64-encoded) and the MIME type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ContentPart {
    /// Plain text content.
    Text { text: String },
    /// Model-facing instruction scaffolding (for example, mode reminders)
    /// appended to user messages. Sent to providers as plain text, but
    /// hidden from chat UIs, which should not render it as user content.
    Reminder { text: String },
    /// An image with base64-encoded data and MIME type (e.g., "image/png").
    Image { data: String, mime_type: String },
    /// An audio file with base64-encoded data and MIME type (e.g., "audio/mp3").
    Audio { data: String, mime_type: String },
    /// A video file with base64-encoded data and MIME type (e.g., "video/mp4").
    Video { data: String, mime_type: String },
    /// A generic file with base64-encoded data and MIME type.
    File { data: String, mime_type: String },
}

impl ContentPart {
    /// Returns the MIME type of this content part.
    ///
    /// For text content, returns "text/plain". For media types, returns the
    /// specific MIME type of that media.
    pub fn mime_type(&self) -> String {
        match self {
            ContentPart::Text { .. } | ContentPart::Reminder { .. } => "text/plain".to_string(),
            ContentPart::Image { mime_type, .. } => mime_type.clone(),
            ContentPart::Audio { mime_type, .. } => mime_type.clone(),
            ContentPart::Video { mime_type, .. } => mime_type.clone(),
            ContentPart::File { mime_type, .. } => mime_type.clone(),
        }
    }
}

/// Describes the capabilities of an LLM model.
///
/// Capabilities indicate what input and output modalities a model supports.
/// This is used to determine how to handle content (e.g., whether to send
/// images directly or convert them to text descriptions).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Capabilities {
    /// Input capabilities - what the model can accept.
    pub input: InputCapabilities,
    /// Output capabilities - what the model can produce.
    pub output: OutputCapabilities,
}

/// Input capabilities for a model.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InputCapabilities {
    /// Whether the model accepts text input.
    pub text: bool,
    /// Whether the model accepts image input.
    pub image: bool,
    /// Whether the model accepts audio input.
    pub audio: bool,
    /// Whether the model accepts video input.
    pub video: bool,
    /// Whether the model accepts PDF input.
    pub pdf: bool,
}

/// Output capabilities for a model.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OutputCapabilities {
    /// Whether the model can produce text output.
    pub text: bool,
    /// Whether the model can produce image output.
    pub image: bool,
    /// Whether the model can produce audio output.
    pub audio: bool,
    /// Whether the model can produce video output.
    pub video: bool,
    /// Whether the model can produce PDF output.
    pub pdf: bool,
}

/// Provider-reported token accounting for one LLM request.
///
/// Every field is provider-reported (`0` when the provider does not report
/// that figure); hit rate is computed against the totals, not per turn.
/// Input token fields follow the Anthropic breakdown; OpenAI's
/// `prompt_tokens_details.cached_tokens` maps to `cached_tokens`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Uncached prompt tokens processed at full price.
    pub input_tokens: u64,
    /// Prompt tokens served from the provider cache (cache hits).
    pub cached_tokens: u64,
    /// Prompt tokens written to the provider cache this request.
    pub cache_write_tokens: u64,
    /// Output (completion) tokens.
    pub output_tokens: u64,
    /// Output tokens the model spent thinking (`reasoning_tokens` in
    /// OpenAI-style breakdowns). Subtracted from nothing: it is reported
    /// for latency diagnostics, not billing. `0` when the provider does
    /// not report it.
    #[serde(default)]
    pub reasoning_tokens: u64,
}

impl TokenUsage {
    /// Adds the other tally into this one (session accumulation).
    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.cached_tokens += other.cached_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
    }

    /// Total prompt tokens this request (or session) processed:
    /// uncached + cached + newly-written.
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens + self.cached_tokens + self.cache_write_tokens
    }

    /// Fraction of prompt tokens served from cache, as a percentage.
    /// `None` when no prompt tokens were processed at all.
    pub fn cache_hit_rate_percent(&self) -> Option<u64> {
        let total = self.prompt_tokens();
        (total > 0).then(|| self.cached_tokens * 100 / total)
    }
}

/// Events emitted by the LLM provider during streaming responses.
///
/// ProviderEvent represents the raw events that come from the LLM provider
/// during a streaming response. These are then transformed into AgentEvent
/// for consumption by the agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderEvent {
    /// Native reasoning state required to replay this assistant response.
    ReasoningReplay { replay: ReasoningReplay },
    /// A delta of reasoning content (e.g., chain-of-thought) from the model.
    ReasoningDelta { delta: String },
    /// A delta of content (text response) from the model.
    ContentDelta { delta: String },
    /// The start of a tool call, providing the tool call ID and function name.
    ToolCallStart { id: String, name: String },
    /// A delta of tool call arguments. Arguments are streamed separately
    /// and may arrive in multiple chunks.
    ToolCallDelta { id: String, delta: String },
    /// The provider has finished generating the response.
    Finish { reason: Option<String> },
    /// Provider-reported token usage for the completed request. Emitted
    /// once per LLM request (after the last usage figure is known).
    Usage { usage: TokenUsage },
    /// An error occurred during streaming.
    Error { error: String },
}

/// Events emitted by the agent during execution.
///
/// AgentEvent represents the high-level events that the agent runtime
/// produces as it processes a request. These events are consumed by
/// the CLI or other frontends to display progress and results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A delta of reasoning content from the model.
    ReasoningDelta { delta: String },
    /// A delta of text content from the model.
    ContentDelta { delta: String },
    /// The agent has started executing a tool.
    ToolStart {
        /// The provider's tool-call ID, also used in the tool history message.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        call_id: String,
        tool: String,
        input: String,
    },
    /// A tool has finished executing.
    ToolFinish {
        /// Same call ID as the corresponding `ToolStart` and history message.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        call_id: String,
        tool: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        title: String,
        output: String,
        #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
        metadata: std::collections::HashMap<String, serde_json::Value>,
    },
    /// The agent has finished with a final response.
    Finish { response: String },
    /// A context-usage snapshot, emitted at each turn boundary so frontends
    /// can update the usage gauge mid-ask.
    Usage {
        /// Estimated tokens in the conversation history (the same estimate
        /// compaction acts on).
        used_tokens: u64,
        /// The agent's context window in tokens.
        context_limit: u64,
        /// Provider-reported token accounting accumulated across the whole
        /// session (every LLM request, including compaction requests).
        /// `None` when the provider reported no usage yet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<TokenUsage>,
    },
    /// A transient status message for frontends (e.g. context compaction).
    ///
    /// Not part of the transcript: frontends render it in their busy
    /// indicator and replace or clear it with the next event.
    Status { message: String },
    /// A provider-side retry wait: the attempt failed and the request
    /// restarts after `delay_secs`.
    ///
    /// Transient like [`AgentEvent::Status`]. The failure's stable identity
    /// ([`RetryKind`]) is separated from the human-readable `message`, so
    /// frontends collapse retries of one failure into a single updating
    /// surface even when error bodies differ per attempt.
    Retrying {
        /// The stable failure identity (e.g. the HTTP status).
        kind: RetryKind,
        /// The full human-readable failure description.
        message: String,
        /// 1-based count of the attempt that just failed.
        attempt: u32,
        /// Whole seconds the retry layer waits before restarting.
        delay_secs: u64,
    },
    /// A live status update from a running tool (e.g. what a subagent is
    /// doing mid-`task`).
    ///
    /// Transient like [`AgentEvent::Status`], but tied to the tool's card:
    /// frontends render it on the still-open tool block and clear it when
    /// the tool finishes. Not part of the transcript.
    ToolProgress { tool: String, message: String },
    /// The agent asked the user a structured question and is waiting for an
    /// answer. Front-ends render a form on the open `question` tool card.
    QuestionAsked {
        /// Server-generated request id (`que_...`), used to reply.
        request_id: String,
        /// Questions in display (and answer) order.
        questions: Vec<crate::question::Question>,
    },
    /// A pending question was answered (`answers` set) or dismissed (`None`).
    QuestionResolved {
        request_id: String,
        answers: Option<Vec<Vec<String>>>,
    },
    /// A steering message was appended to the running conversation.
    ///
    /// Emitted when a message sent through the agent's
    /// [`Steering`](crate::agent::Steering) handle is injected into history
    /// (while the agent was idle, between tool rounds, or during the final
    /// response). Part of the transcript: frontends render it as a user
    /// message at this point of the stream.
    Steered { text: String },
    /// A compaction checkpoint was durably stored; recent context stays internal.
    Compacted {
        #[serde(default)]
        summary: String,
    },
    /// An error occurred during agent execution.
    Error { error: String },
}

impl AgentEvent {
    /// The static prefix of the retry line — the failure description and the
    /// "retrying in" label. Browser frontends append the live countdown tail
    /// (`"{n}s (attempt {m})"`) so the wording has one source.
    pub fn retry_prefix(message: &str) -> String {
        format!("LLM request failed: {message}; retrying in")
    }

    /// The human-readable line for an [`AgentEvent::Retrying`] notification,
    /// shared by every frontend so the wording has one source.
    pub fn retry_message(message: &str, attempt: u32, delay_secs: u64) -> String {
        format!(
            "{} {}s (attempt {attempt})",
            Self::retry_prefix(message),
            delay_secs
        )
    }
}

/// The stable identity of a provider-side retry, separate from the volatile
/// human-readable message: gateways embed per-request details in error
/// bodies, so identity must not be derived from the text.
///
/// Serializes as a flat string tag (`"status-502"`, `"rate-limit"`, …) so
/// browser frontends key on a plain value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryKind {
    /// An HTTP status returned by the server (5xx, or a 408 timeout).
    Status(u16),
    /// The provider rejected the request as a rate limit (429).
    RateLimit,
    /// A failure inside the streamed response (an in-flight error event or
    /// a dropped connection mid-stream).
    Stream,
    /// A transport-level failure (connect errors, read timeouts).
    Transport,
    /// The streamed body could not be parsed.
    Parse,
    /// An unclassifiable failure; no stable identity finer than this exists.
    Other,
}

impl RetryKind {
    fn tag(&self) -> String {
        match self {
            RetryKind::Status(status) => format!("status-{status}"),
            RetryKind::RateLimit => "rate-limit".to_string(),
            RetryKind::Stream => "stream".to_string(),
            RetryKind::Transport => "transport".to_string(),
            RetryKind::Parse => "parse".to_string(),
            RetryKind::Other => "other".to_string(),
        }
    }

    fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "rate-limit" => return Some(RetryKind::RateLimit),
            "stream" => return Some(RetryKind::Stream),
            "transport" => return Some(RetryKind::Transport),
            "parse" => return Some(RetryKind::Parse),
            "other" => return Some(RetryKind::Other),
            _ => {}
        }
        tag.strip_prefix("status-")
            .and_then(|code| code.parse().ok())
            .map(RetryKind::Status)
    }
}

impl std::fmt::Display for RetryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.tag())
    }
}

impl Serialize for RetryKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.tag())
    }
}

impl<'de> Deserialize<'de> for RetryKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let tag = String::deserialize(deserializer)?;
        Self::from_tag(&tag)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown retry kind '{tag}'")))
    }
}

/// A message in the conversation history.
///
/// Message represents a single turn in the conversation. Messages have
/// different roles that affect how they are processed:
///
/// - **System**: Configuration/instructions for the agent. Typically the first message.
/// - **User**: Input from the human user.
/// - **Assistant**: Response from the agent. May contain text content and/or tool calls.
/// - **Tool**: Result from executing a tool. Includes the tool call ID to link it
///   to the corresponding tool call in the assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    /// Durable context boundary. Earlier messages remain in the transcript,
    /// but only this checkpoint and subsequent messages reach the model.
    Compaction { summary: String, recent: String },
    /// A system message, typically used to set agent behavior/instructions.
    System {
        /// The content of the system message.
        content: String,
    },
    /// A message from the user.
    User {
        /// The content parts of the user message.
        content: Vec<ContentPart>,
    },
    /// A message from the assistant (the agent).
    Assistant {
        /// The text content of the assistant's response, if any.
        content: Option<String>,
        /// Reasoning (chain-of-thought) text streamed before the answer, if any.
        reasoning: Option<String>,
        /// Provider-native replay state; opaque values are never displayed.
        replay: Option<ReasoningReplay>,
        /// Tool calls requested by the assistant, if any.
        tool_calls: Option<Vec<ToolCall>>,
    },
    /// A message containing the result of a tool execution.
    Tool {
        /// The ID of the tool call this result corresponds to.
        tool_call_id: String,
        /// The name of the tool that was executed, if known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
        /// The result content from the tool execution.
        content: String,
        /// Any additional media returned by the tool (e.g., images from reading a file).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        media: Vec<ContentPart>,
    },
}

/// Replay state belongs to exactly one endpoint and model. Anthropic stores
/// its ordered native assistant blocks, including thinking signatures. OpenAI
/// Responses stores output items, including encrypted reasoning and call IDs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningReplay {
    pub origin: String,
    pub model: String,
    pub field: Option<String>,
    pub blocks: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileAttachment {
    pub path: PathBuf,
    pub mime: Option<String>,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolResult {
    pub title: String,
    pub output: String,
    #[serde(default)]
    pub metadata: std::collections::HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub attachments: Vec<FileAttachment>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_path: Option<PathBuf>,
}

impl ToolResult {
    pub fn new(title: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            output: output.into(),
            metadata: Default::default(),
            attachments: Default::default(),
            truncated: false,
            output_path: None,
        }
    }
}

/// How strictly file-touching tools confine paths to the working directory.
///
/// The level is server configuration (config key `sandbox`), travels to tools
/// on [`ToolContext`], and defaults to [`PathEnforcement::None`] — paths are
/// used as provided with no containment checks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathEnforcement {
    /// No checks: relative paths are joined to the working directory and
    /// absolute paths are used as-is.
    #[default]
    None,
    /// Lexical containment: `.`/`..` are normalized and paths that resolve
    /// outside the working directory are rejected. Symlinks are followed.
    Contain,
    /// Containment plus canonicalization and symlink-component rejection;
    /// resolved paths must land inside the real (canonicalized) root.
    Strict,
}

/// Typed handle to per-session key/value state shared by tools.
///
/// Tools receive a clone of this handle on every [`ToolContext`], keeping the
/// tools themselves stateless. Values are stored as JSON strings under a
/// tool-chosen key (e.g. `"todos"`) scoped to the current session, so state
/// survives process restarts when a persistent store is wired in.
#[derive(Clone)]
pub struct SessionState {
    store: Arc<dyn SessionStateStore>,
}

impl std::fmt::Debug for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionState").finish_non_exhaustive()
    }
}

impl SessionState {
    /// Creates a handle backed by the given store.
    pub fn new(store: Arc<dyn SessionStateStore>) -> Self {
        Self { store }
    }

    /// Creates a handle backed by a fresh in-memory store.
    ///
    /// This is the default for [`ToolContext::new`]; persistent backends
    /// inject themselves via [`ToolContext::with_session_state`].
    pub fn in_memory() -> Self {
        Self::new(Arc::new(InMemorySessionStateStore::default()))
    }

    /// Reads and deserializes the value stored for `key`, if any.
    pub async fn read<T: DeserializeOwned>(
        &self,
        session_id: &str,
        key: &str,
    ) -> anyhow::Result<Option<T>> {
        match self.store.load(session_id, key).await? {
            Some(value) => Ok(Some(serde_json::from_str(&value)?)),
            None => Ok(None),
        }
    }

    /// Serializes and stores `value` for `key`, replacing any previous value.
    pub async fn write<T: Serialize>(
        &self,
        session_id: &str,
        key: &str,
        value: &T,
    ) -> anyhow::Result<()> {
        let value = serde_json::to_string(value)?;
        self.store.save(session_id, key, value).await
    }
}

/// In-memory [`SessionStateStore`], the default when no persistent backend
/// is wired in. State lives for as long as a clone of the store is held.
#[derive(Default)]
pub struct InMemorySessionStateStore {
    entries: std::sync::Mutex<std::collections::HashMap<(String, String), String>>,
}

#[async_trait]
impl SessionStateStore for InMemorySessionStateStore {
    async fn load(&self, session_id: &str, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .entries
            .lock()
            .expect("session state mutex poisoned")
            .get(&(session_id.to_string(), key.to_string()))
            .cloned())
    }

    async fn save(&self, session_id: &str, key: &str, value: String) -> anyhow::Result<()> {
        self.entries
            .lock()
            .expect("session state mutex poisoned")
            .insert((session_id.to_string(), key.to_string()), value);
        Ok(())
    }
}

/// Outbound progress channel a long-running tool uses to surface live
/// status (`AgentEvent::ToolProgress`) while it executes. The agent loop
/// wires one per tool call, bound to the tool's name; tools report through
/// [`ToolContext::report_progress`].
#[derive(Clone, Debug)]
pub struct ProgressSink {
    tool: String,
    sender: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
}

impl ProgressSink {
    pub fn new(
        tool: impl Into<String>,
        sender: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
    ) -> Self {
        Self {
            tool: tool.into(),
            sender,
        }
    }

    /// Queues a status update; a closed channel (the turn already ended)
    /// drops the message silently.
    pub fn send(&self, message: impl Into<String>) {
        let _ = self.sender.send(AgentEvent::ToolProgress {
            tool: self.tool.clone(),
            message: message.into(),
        });
    }

    /// Sends a raw event on the tool's channel. Used for events tied to the
    /// tool call that are not plain progress text.
    pub fn send_event(&self, event: AgentEvent) {
        let _ = self.sender.send(event);
    }
}

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub session_id: String,
    pub message_id: String,
    pub abort: Arc<std::sync::atomic::AtomicBool>,
    pub metadata: std::collections::HashMap<String, serde_json::Value>,
    /// File-path confinement level for this execution.
    pub enforcement: PathEnforcement,
    /// Per-session key/value state tools can persist across turns.
    pub session_state: SessionState,
    pub inbox: Option<crate::agent::Steering>,
    /// Live-status sink for this tool call; `None` when the host does not
    /// consume progress events.
    pub progress: Option<ProgressSink>,
    /// The session's selected workspace root. `None` keeps compatibility with
    /// direct tool callers, which use the tool's constructor fallback root.
    pub working_directory: Option<PathBuf>,
}

impl ToolContext {
    pub fn new(session_id: impl Into<String>, message_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            message_id: message_id.into(),
            abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            metadata: Default::default(),
            enforcement: PathEnforcement::default(),
            session_state: SessionState::in_memory(),
            inbox: None,
            progress: None,
            working_directory: None,
        }
    }

    /// Reports a transient status update for this tool call (rendered on
    /// the tool's card by frontends). A no-op when no sink is wired.
    pub fn report_progress(&self, message: impl Into<String>) {
        if let Some(sink) = &self.progress {
            sink.send(message);
        }
    }

    /// Announces a structured question on this tool call's card. The request
    /// must already be registered with the question broker, so a front-end
    /// that answers immediately cannot race the registration.
    pub fn report_question_asked(
        &self,
        request_id: impl Into<String>,
        questions: Vec<crate::question::Question>,
    ) {
        if let Some(sink) = &self.progress {
            sink.send_event(AgentEvent::QuestionAsked {
                request_id: request_id.into(),
                questions,
            });
        }
    }

    /// Settles a previously announced question on this tool call's card.
    /// `None` means the user dismissed it.
    pub fn report_question_resolved(
        &self,
        request_id: impl Into<String>,
        answers: Option<Vec<Vec<String>>>,
    ) {
        if let Some(sink) = &self.progress {
            sink.send_event(AgentEvent::QuestionResolved {
                request_id: request_id.into(),
                answers,
            });
        }
    }

    /// Returns the context with a different path-enforcement level.
    pub fn with_enforcement(mut self, enforcement: PathEnforcement) -> Self {
        self.enforcement = enforcement;
        self
    }

    /// Returns the context backed by a different session-state store.
    pub fn with_session_state(mut self, session_state: SessionState) -> Self {
        self.session_state = session_state;
        self
    }

    /// Selects the workspace root for this tool call.
    pub fn with_working_directory(mut self, working_directory: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(working_directory.into());
        self
    }

    /// Returns the selected workspace root, or the tool's legacy fallback.
    pub fn active_working_directory<'a>(&'a self, fallback: &'a Path) -> &'a Path {
        self.working_directory.as_deref().unwrap_or(fallback)
    }

    pub fn is_aborted(&self) -> bool {
        self.abort.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn abort(&self) {
        self.abort.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[derive(Clone, Debug)]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub output_path: Option<PathBuf>,
}

/// Collapse tool input JSON to a single short line for display, shared by
/// the frontends (tool blocks) and tools that surface sub-activity (the
/// task tool's live progress).
pub fn summarize_tool_input(tool: &str, input: &str) -> String {
    let value: serde_json::Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => return truncate_line(input, 80),
    };
    let summary = match tool {
        "bash" => value
            .get("command")
            .and_then(|c| c.as_str())
            .map(str::to_string),
        "read_file" | "write_file" | "edit_file" | "apply_patch" => value
            .get("file_path")
            .and_then(|p| p.as_str())
            .map(str::to_string),
        "glob" | "list" | "grep" => value
            .get("pattern")
            .and_then(|p| p.as_str())
            .map(str::to_string),
        "webfetch" => value
            .get("url")
            .and_then(|u| u.as_str())
            .map(str::to_string),
        "websearch" => value
            .get("query")
            .and_then(|q| q.as_str())
            .map(str::to_string),
        "task" => value
            .get("description")
            .and_then(|d| d.as_str())
            .map(str::to_string),
        "skill" => value
            .get("name")
            .and_then(|n| n.as_str())
            .map(str::to_string),
        // The card renders the form from the question event, so the summary
        // only needs the first tab's label.
        "question" => value
            .get("questions")
            .and_then(|questions| questions.as_array())
            .and_then(|questions| questions.first())
            .and_then(|question| {
                question
                    .get("header")
                    .or_else(|| question.get("question"))
                    .and_then(|label| label.as_str())
            })
            .map(|label| label.trim().to_string()),
        // The GUI renders the workflow graph from the spec, so keep it whole.
        "workflow_start" => value.get("spec").map(|spec| spec.to_string()),
        "workflow_status" | "workflow_cancel" => value
            .get("run_id")
            .and_then(|id| id.as_str())
            .map(str::to_string),
        _ => None,
    };
    summary.unwrap_or_else(|| truncate_line(&value.to_string(), 80))
}

/// One-line truncation for tool input summaries.
fn truncate_line(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out.replace('\n', " ")
}

/// Represents a stored experience.
///
/// An experience records a problem-solving episode that can be
/// retrieved later to inform future similar tasks.
#[derive(Debug, Clone)]
pub struct Experience {
    pub id: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

/// Query parameters for experience recall.
///
/// Used to search for experiences similar to a given query text.
#[derive(Debug, Clone)]
pub struct ExperienceQuery {
    pub query_text: String,
    pub limit: usize,
}

/// A recalled experience with its similarity score.
///
/// Returned by [`Experiences::recall`](super::traits::Experiences::recall) to provide the matching
/// experience along with a similarity score indicating relevance.
#[derive(Debug, Clone)]
pub struct RecalledExperience {
    pub experience: Experience,
    pub similarity_score: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reminder parts persist in stored history, so they must round-trip
    /// through the internally-tagged JSON representation with their own
    /// type tag (distinct from plain text).
    #[test]
    fn reminder_part_round_trips_with_distinct_type_tag() {
        let part = ContentPart::Reminder {
            text: "<system-reminder>Plan mode is active.</system-reminder>".to_string(),
        };
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json["type"], "reminder");
        let back: ContentPart = serde_json::from_value(json).unwrap();
        assert_eq!(back, part);
    }

    #[test]
    fn tool_context_can_override_the_compatibility_root() {
        let root = std::path::PathBuf::from("/tmp/selected-root");
        let fallback = std::path::PathBuf::from("/tmp/fallback-root");
        let ctx = ToolContext::new("session", "message").with_working_directory(root.clone());
        assert_eq!(ctx.active_working_directory(&fallback), root.as_path());
    }
}
