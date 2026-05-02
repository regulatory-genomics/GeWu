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
//! - [`ExecutionRun`] / [`ExecutionEvent`] - Observable execution trace records
//! - [`FileAttachment`] - File attachment in tool results
//! - [`TruncationResult`] - Result of output truncation

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Represents a function call within a tool call.
///
/// Contains the name of the function to invoke and its arguments as a JSON string.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ContentPart {
    /// Plain text content.
    Text { text: String },
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
            ContentPart::Text { .. } => "text/plain".to_string(),
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

/// Events emitted by the LLM provider during streaming responses.
///
/// ProviderEvent represents the raw events that come from the LLM provider
/// during a streaming response. These are then transformed into AgentEvent
/// for consumption by the agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderEvent {
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
    /// An error occurred during streaming.
    Error { error: String },
}

/// Events emitted by the agent during execution.
///
/// AgentEvent represents the high-level events that the agent runtime
/// produces as it processes a request. These events are consumed by
/// the CLI or other frontends to display progress and results.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A delta of reasoning content from the model.
    ReasoningDelta { delta: String },
    /// A delta of text content from the model.
    ContentDelta { delta: String },
    /// The agent has started executing a tool.
    ToolStart {
        tool_call_id: String,
        tool: String,
        input: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        metadata: serde_json::Value,
    },
    /// A tool has finished executing.
    ToolFinish {
        tool_call_id: String,
        tool: String,
        output: String,
    },
    /// The agent has finished with a final response.
    Finish { response: String },
    /// An error occurred during agent execution.
    Error { error: String },
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolExecutionPolicy {
    Serial,
    ParallelSafe,
    Exclusive,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRunKind {
    ChatMessage,
    Tool,
    Workflow,
    WorkflowNode,
    Specialist,
}

impl ExecutionRunKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ChatMessage => "chat_message",
            Self::Tool => "tool",
            Self::Workflow => "workflow",
            Self::WorkflowNode => "workflow_node",
            Self::Specialist => "specialist",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "chat_message" => Some(Self::ChatMessage),
            "tool" => Some(Self::Tool),
            "workflow" => Some(Self::Workflow),
            "workflow_node" => Some(Self::WorkflowNode),
            "specialist" => Some(Self::Specialist),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRunStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Skipped,
}

impl ExecutionRunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ExecutionRun {
    pub id: String,
    pub parent_id: Option<String>,
    pub session_id: Option<String>,
    pub kind: ExecutionRunKind,
    pub name: String,
    pub status: ExecutionRunStatus,
    pub input_json: Option<serde_json::Value>,
    pub output_json: Option<serde_json::Value>,
    pub error: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ExecutionEvent {
    pub id: String,
    pub run_id: String,
    pub event_type: String,
    pub payload_json: serde_json::Value,
    pub created_at: i64,
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

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub session_id: String,
    pub message_id: String,
    pub tool_call_id: Option<String>,
    pub abort: Arc<std::sync::atomic::AtomicBool>,
    pub metadata: std::collections::HashMap<String, serde_json::Value>,
}

impl ToolContext {
    pub fn new(session_id: impl Into<String>, message_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            message_id: message_id.into(),
            tool_call_id: None,
            abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            metadata: Default::default(),
        }
    }

    pub fn with_tool_call_id(mut self, tool_call_id: impl Into<String>) -> Self {
        self.tool_call_id = Some(tool_call_id.into());
        self
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

// ---------------------------------------------------------------------------
// Capability / tool-provider types
// ---------------------------------------------------------------------------

/// Metadata describing a capability (logical tool) exposed by a `ToolProvider`.
///
/// Capability names are the string ids referenced by specialists. A descriptor
/// carries the human-readable description, an optional JSON schema for the
/// tool's input, and a provider-kind tag ("builtin", "plugin", "cli", "script",
/// "mcp", "http", "python", "container", "database", "graph").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityDescriptor {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub provider_kind: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub input_schema: serde_json::Value,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

impl CapabilityDescriptor {
    pub fn new(name: impl Into<String>, provider_kind: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            provider_kind: provider_kind.into(),
            input_schema: serde_json::Value::Null,
            metadata: serde_json::Value::Null,
        }
    }
}

// ---------------------------------------------------------------------------
// Specialist assembly types
// ---------------------------------------------------------------------------

/// Declarative description of a specialist agent.
///
/// Used by both builtins and plugins — one type, one assembly path.
/// Capabilities are string-based IDs resolved by `ToolFactory` implementations
/// at assembly time.
///
/// Well-known builtin capability IDs:
/// `"read_file"`, `"write_file"`, `"edit_file"`, `"glob"`, `"bash"`,
/// `"bash_safe"`, `"grep"`, `"web_fetch"`, `"web_search"`, `"list"`,
/// `"code_search"`, `"todo"`, `"task"`, `"report_result"`
#[derive(Debug, Clone)]
pub struct SpecialistDescriptor {
    pub id: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub skills: Vec<String>,
    pub system_prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecialistExecutionRequest {
    pub task_id: String,
    pub specialist_id: String,
    pub session_id: String,
    pub prompt: String,
    #[serde(default)]
    pub metadata_json: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionRecord {
    pub tool_name: String,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecialistAgentEventRecord {
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    pub payload_json: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecialistLlmIoRecord {
    pub system_prompt: String,
    pub user_prompt: String,
    pub final_response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecialistExecutionEnvelope {
    pub task_id: String,
    pub session_id: String,
    pub specialist_id: String,
    pub status: TaskStatus,
    pub attempt: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_json: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_outputs: Vec<ToolExecutionRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<SpecialistAgentEventRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub llm_io: Vec<SpecialistLlmIoRecord>,
}

// ---------------------------------------------------------------------------
// Durable task types
// ---------------------------------------------------------------------------

/// Lifecycle status of a durable task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Ready,
    Running,
    Blocked,
    Succeeded,
    Failed,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "ready" => Some(Self::Ready),
            "running" => Some(Self::Running),
            "blocked" => Some(Self::Blocked),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// Runtime-level task execution state. This answers whether execution is still
/// running, ended normally, or ended through an operational failure/cancel path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskExecutionStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Canceled,
    Timeout,
    Interrupted,
}

impl TaskExecutionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Timeout => "timeout",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "canceled" => Some(Self::Canceled),
            "timeout" => Some(Self::Timeout),
            "interrupted" => Some(Self::Interrupted),
            _ => None,
        }
    }
}

/// Result-level task quality state. This answers whether a completed execution
/// actually produced a usable task result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskResultStatus {
    Pending,
    Accepted,
    Empty,
    Incomplete,
    Invalid,
    NeedsReview,
}

impl TaskResultStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Empty => "empty",
            Self::Incomplete => "incomplete",
            Self::Invalid => "invalid",
            Self::NeedsReview => "needs_review",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "accepted" => Some(Self::Accepted),
            "empty" => Some(Self::Empty),
            "incomplete" => Some(Self::Incomplete),
            "invalid" => Some(Self::Invalid),
            "needs_review" => Some(Self::NeedsReview),
            _ => None,
        }
    }
}

/// Persisted record for a durable task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_id: String,
    pub specialist_id: String,
    pub prompt: String,
    pub status: TaskStatus,
    pub execution_status: TaskExecutionStatus,
    pub result_status: TaskResultStatus,
    pub attempt: u32,
    pub block_reason: Option<String>,
    pub result_json: Option<String>,
    pub finish_reason: Option<String>,
    pub error: Option<String>,
    pub parent_session_id: Option<String>,
    pub child_session_id: Option<String>,
    pub root_run_id: Option<String>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}
