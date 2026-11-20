//! Core traits for the GeWu agent framework.
//!
//! This module defines the fundamental traits that must be implemented to
//! extend GeWu with custom tools, LLM providers, and memory backends:
//!
//! - [`Tool`] - Implement to create custom tools that the agent can call
//! - [`LlmProvider`] - Implement to add support for different LLM backends
//! - [`ToolRegistry`] - Implement to provide a collection of tools to the agent
//! - [`Memory`] - Implement to add custom conversation history storage
//! - [`SessionStateStore`] - Implement to persist per-session tool state
//!
//! # Example: Implementing a Custom Tool
//!
//! ```ignore
//! use async_trait::async_trait;
//! use gewu_core::traits::Tool;
//! use gewu_core::types::{ToolResult, ToolContext};
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(JsonSchema, Deserialize)]
//! struct MyParams {
//!     input: String,
//! }
//!
//! struct MyTool;
//!
//! #[async_trait]
//! impl Tool for MyTool {
//!     fn name(&self) -> &str { "my_tool" }
//!
//!     fn description(&self) -> &str { "A description of what my tool does" }
//!
//!     fn schema() -> schemars::schema::RootSchema {
//!         schemars::schema_for!(MyParams)
//!     }
//!
//!     async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
//!         let params: MyParams = serde_json::from_value(args)?;
//!         Ok(ToolResult::new("my_tool", format!("result: {}", params.input)))
//!     }
//! }
//! ```

use crate::types::{
    ExperienceQuery, Message, ProviderEvent, RecalledExperience, TokenUsage, ToolContext,
    ToolResult,
};
use async_trait::async_trait;
use chrono::Utc;
use futures::StreamExt;
use futures::stream::BoxStream;
use schemars::schema::RootSchema;
use std::error::Error;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::warn;
use uuid::Uuid;

/// Defines a tool that the agent can call during execution.
///
/// The `Tool` trait is the core extension point for GeWu. Implement this trait
/// to create custom tools that the agent can invoke via the LLM.
///
/// # Example
///
/// ```
/// use async_trait::async_trait;
/// use std::error::Error;
/// use gewu_core::traits::Tool;
/// use gewu_core::types::{ToolResult, ToolContext};
/// use schemars::schema::RootSchema;
/// use schemars::JsonSchema;
/// use serde::Deserialize;
///
/// #[derive(JsonSchema, Deserialize)]
/// struct MyParams {
///     input: String,
/// }
///
/// struct MyTool;
///
/// #[async_trait]
/// impl Tool for MyTool {
///     fn name(&self) -> &str { "my_tool" }
///     fn description(&self) -> &str { "A description of what my tool does" }
///     fn schema(&self) -> RootSchema { schemars::schema_for!(MyParams) }
///     async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
///         let params: MyParams = serde_json::from_value(args)?;
///         Ok(ToolResult::new("my_tool", format!("result: {}", params.input)))
///     }
/// }
/// ```
#[async_trait]
pub trait Tool: Send + Sync {
    /// Returns the unique name of this tool.
    ///
    /// This name is used by the LLM to request this tool and must be
    /// unique among all tools registered with the agent.
    fn name(&self) -> &str;

    /// Returns a description of what this tool does.
    ///
    /// The description is passed to the LLM so it can decide when to
    /// call this tool based on user requests.
    fn description(&self) -> &str;

    /// Returns the JSON schema for this tool's parameters.
    ///
    /// The schema is used by the LLM to generate valid arguments
    /// when calling this tool.
    fn schema(&self) -> RootSchema;

    /// Initializes the tool (optional).
    ///
    /// Called once before the tool is first used. Override to perform
    /// setup such as opening connections or loading resources.
    async fn init(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }

    /// Executes the tool with the given arguments.
    ///
    /// # Arguments
    ///
    /// * `args` - JSON value containing the tool arguments (validated against the schema)
    /// * `ctx` - Execution context containing session ID, message ID, and abort signal
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>>;
}

/// Provides access to a collection of tools for execution.
///
/// The `ToolRegistry` trait is implemented by types that manage multiple tools,
/// such as the `Agent` itself. This abstraction allows tools to be loaded
/// dynamically and executed through a unified interface.
#[async_trait]
pub trait ToolRegistry: Send + Sync {
    /// Executes a tool by name with the given arguments.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the tool to execute
    /// * `args` - JSON arguments to pass to the tool
    /// * `ctx` - Execution context
    async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>>;

    /// Returns a list of all available tool names.
    fn list_tools(&self) -> Vec<String>;
}

/// Shareable handle to a request-log writer.
///
/// The writer is wrapped in `Arc<Mutex<...>>` so `LlmProvider::stream_response`
/// can take `&self` and one handle can be shared across agents.
pub type RequestLog = Arc<Mutex<Box<dyn Write + Send + Sync>>>;

/// A provider-side retry wait: one request attempt failed and the request
/// will restart after `delay`.
///
/// Providers report these through [`StatusSink`]; the agent layer forwards
/// them to frontends as [`crate::types::AgentEvent::Retrying`] so a backoff
/// wait is visible instead of looking like a silent hang. Frontends collapse
/// retries of one [`kind`](crate::types::RetryKind) into a single updating surface.
#[derive(Debug, Clone)]
pub struct RetryNotice {
    /// The stable failure identity, separate from [`Self::message`]: gateways
    /// embed per-request details in error bodies, so identity must not be
    /// derived from the text.
    pub kind: crate::types::RetryKind,
    /// The full human-readable failure description (the provider error's
    /// Display form).
    pub message: String,
    /// 1-based count of the attempt that just failed.
    pub attempt: u32,
    /// How long the retry layer waits before restarting.
    pub delay: std::time::Duration,
}

/// Extracts the stable retry identity from a provider error.
///
/// Implemented by the provider error enums; the defaulted method keeps the
/// generic retry loops usable with any error type.
pub trait RetryClassify {
    fn retry_kind(&self) -> crate::types::RetryKind {
        crate::types::RetryKind::Other
    }
}

impl RetryClassify for String {}

// The generic retry loops are also exercised with boxed and plain-string
// test errors; both classify as [`crate::types::RetryKind::Other`].
impl<T: RetryClassify + ?Sized> RetryClassify for Box<T> {}
impl RetryClassify for dyn std::error::Error + Send + Sync {}

/// Callback for transient provider status updates (retry backoff).
///
/// Providers invoke it whenever a request attempt fails and will be retried;
/// the agent layer forwards these to frontends as
/// [`crate::types::AgentEvent::Retrying`].
pub type StatusSink = std::sync::Arc<dyn Fn(RetryNotice) + Send + Sync>;

/// Timing summary of one finished LLM response, for the wire log.
struct ResponseTiming {
    /// `"ok"` when the stream ended normally, `"error"` on a stream error.
    outcome: &'static str,
    /// Send → first event. `None` when no event arrived.
    ttft_ms: Option<u64>,
    /// Send → stream end (or stream error).
    total_ms: u64,
    /// Provider events consumed.
    events: usize,
    /// Error message when `outcome` is `"error"`.
    error: Option<String>,
    /// Provider-reported usage, when the terminal chunk carried one.
    usage: Option<TokenUsage>,
}

/// Records one wire request as a JSONL line. Generic across providers.
///
/// Best-effort: write failures emit a warning and never fail the request.
fn log_wire_request(log: &RequestLog, id: &str, request: &serde_json::Value) {
    let entry = serde_json::json!({
        "id": id,
        "timestamp": Utc::now().to_rfc3339(),
        "request": request,
    });
    if let Ok(mut writer) = log.lock()
        && let Err(e) = writeln!(writer, "{entry}").and_then(|()| writer.flush())
    {
        warn!("Failed to write LLM request log: {}", e);
    }
}

/// Records one finished LLM response's timing summary as a JSONL line.
///
/// Paired with its request line via `id`. Best-effort, like
/// [`log_wire_request`]: write failures warn and never fail the turn.
fn log_wire_response(log: &RequestLog, id: &str, timing: ResponseTiming) {
    let entry = serde_json::json!({
        "id": id,
        "timestamp": Utc::now().to_rfc3339(),
        "response": {
            "ttft_ms": timing.ttft_ms,
            "total_ms": timing.total_ms,
            "events": timing.events,
            "outcome": timing.outcome,
            "error": timing.error,
            "usage": timing.usage,
        },
    });
    if let Ok(mut writer) = log.lock()
        && let Err(e) = writeln!(writer, "{entry}").and_then(|()| writer.flush())
    {
        warn!("Failed to write LLM response log: {}", e);
    }
}

/// Wraps a provider stream so its timing and usage are recorded when the
/// stream finishes.
///
/// `started` should be captured before the request is sent, so TTFT covers
/// the full silent window (connection + provider think time). Events pass
/// through unchanged and in order; the summary line is written after the
/// last event (or the error). Errors still propagate to the consumer
/// exactly as before.
fn record_response_timing<'a>(
    log: &RequestLog,
    id: String,
    started: Instant,
    stream: BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
) -> BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>> {
    let log = log.clone();
    let wrapped = async_stream::stream! {
        let mut ttft_ms: Option<u64> = None;
        let mut events = 0usize;
        let mut usage: Option<TokenUsage> = None;
        let mut stream = Box::pin(stream);
        loop {
            match stream.as_mut().next().await {
                Some(Ok(event)) => {
                    if ttft_ms.is_none() {
                        ttft_ms = Some(started.elapsed().as_millis() as u64);
                    }
                    events += 1;
                    // Usage rides on the terminal chunk; peek it for the
                    // summary and pass the event through untouched.
                    if let ProviderEvent::Usage { usage: reported } = &event {
                        usage = Some(*reported);
                    }
                    yield Ok(event);
                }
                Some(Err(e)) => {
                    log_wire_response(&log, &id, ResponseTiming {
                        outcome: "error",
                        ttft_ms,
                        total_ms: started.elapsed().as_millis() as u64,
                        events,
                        error: Some(e.to_string()),
                        usage,
                    });
                    yield Err(e);
                    break;
                }
                None => {
                    log_wire_response(&log, &id, ResponseTiming {
                        outcome: "ok",
                        ttft_ms,
                        total_ms: started.elapsed().as_millis() as u64,
                        events,
                        error: None,
                        usage,
                    });
                    break;
                }
            }
        }
    };
    Box::pin(wrapped)
}

/// Provides access to a language model for generating responses.
///
/// The `LlmProvider` trait abstracts over different LLM backends (OpenAI, Anthropic, etc.).
/// Implement this trait to add support for new LLM providers.
///
/// Message building and message sending are isolated: `build` converts core
/// messages and tools into the provider's wire request, and `send` delivers it.
/// `stream_response` is generic across providers: it builds the request, logs
/// it when a request log is supplied, then sends it.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Initializes the provider (optional).
    ///
    /// Called once before the provider is first used. Override to perform
    /// setup such as validating credentials or checking API availability.
    async fn init(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>
    where
        Self: Sized,
    {
        Ok(())
    }

    /// Builds the provider-specific wire request from core messages and tools.
    ///
    /// The returned JSON value is the exact request body sent to the
    /// provider's API, and what gets recorded when a request log is supplied.
    fn build(
        &self,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
    ) -> Result<serde_json::Value, Box<dyn Error + Send + Sync>>;

    /// Sends a built wire request and streams the provider's response.
    async fn send<'a>(
        &'a self,
        request: serde_json::Value,
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    >;

    /// Streams a response from the LLM: build -> log -> send.
    ///
    /// # Arguments
    ///
    /// * `messages` - The conversation history to send to the LLM
    /// * `tools` - Available tools the LLM can call
    /// * `log` - Optional request log; when supplied, the built wire request
    ///   is recorded as one JSONL line before sending, and a timing/usage
    ///   summary line is recorded when the response stream finishes. The two
    ///   lines share an `id` for correlation.
    ///
    /// # Returns
    ///
    /// A stream of provider events representing the LLM's response
    async fn stream_response<'a>(
        &'a self,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
        log: Option<&RequestLog>,
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        let request = self.build(messages, tools)?;
        let request_id = Uuid::new_v4().to_string();
        if let Some(log) = log {
            log_wire_request(log, &request_id, &request);
        }
        // The clock starts before `send` so TTFT includes connection
        // establishment, TLS, and the provider's think time — the whole
        // silent window a user experiences, not just body streaming.
        let started = Instant::now();
        let stream = self.send(request).await?;
        Ok(match log {
            Some(log) => record_response_timing(log, request_id, started, stream),
            None => stream,
        })
    }

    /// Registers a sink for transient status messages (e.g. retry backoff).
    ///
    /// Called by the agent layer before streaming. Providers with retry
    /// loops report each failed attempt through it; the default does
    /// nothing (useful for test doubles).
    fn set_status_sink(&self, _sink: Option<StatusSink>) {}

    /// Lists available models for this provider.
    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>>;
}

/// Stores and manages conversation history.
///
/// The `Memory` trait abstracts over different storage backends for
/// conversation history. Implement this trait to add custom storage
/// solutions such as databases or persistent storage.
#[async_trait]
pub trait Memory: Send + Sync {
    /// Initializes the memory store (optional).
    async fn init(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>
    where
        Self: Sized,
    {
        Ok(())
    }

    /// Returns a slice of all messages in the conversation history.
    fn messages(&self) -> &[Message];

    /// Adds a message to the conversation history.
    async fn add_message(&mut self, message: Message);

    /// Persist a completed checkpoint before activating its context boundary.
    /// Backends that can fail writes must override this and propagate errors.
    async fn add_checkpoint(&mut self, summary: String, recent: String) -> anyhow::Result<()> {
        self.add_message(Message::Compaction { summary, recent })
            .await;
        Ok(())
    }

    /// Clears all messages from the conversation history.
    async fn clear(&mut self);

    /// Replaces all messages with a new set of messages.
    async fn replace(&mut self, messages: Vec<Message>);
}

/// In-memory implementation of `Memory` using a `Vec<Message>`.
#[async_trait]
impl Memory for Vec<Message> {
    async fn init(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>
    where
        Self: Sized,
    {
        Ok(())
    }

    fn messages(&self) -> &[Message] {
        self
    }

    async fn add_message(&mut self, message: Message) {
        self.push(message);
    }

    async fn clear(&mut self) {
        self.clear();
    }

    async fn replace(&mut self, messages: Vec<Message>) {
        *self = messages;
    }
}

/// Abstraction for storing and retrieving experiences.
///
/// The `Experiences` trait provides a simple interface for recording
/// and retrieving past experiences to inform future problem-solving.
///
/// When used with an [`Agent`] via
/// [`AgentBuilder::with_experiences`](crate::agent::AgentBuilder::with_experiences),
/// a `search_experiences` tool is automatically registered, allowing the LLM
/// to recall relevant past experiences during problem-solving.
///
/// [`Agent`]: crate::agent::Agent
#[async_trait]
pub trait Experiences: Send + Sync {
    /// Stores a new experience and returns its unique ID.
    async fn store(&self, content: &str) -> Result<String, Box<dyn Error + Send + Sync>>;

    /// Recalls experiences similar to the query, sorted by similarity (highest first).
    async fn recall(
        &self,
        query: &ExperienceQuery,
    ) -> Result<Vec<RecalledExperience>, Box<dyn Error + Send + Sync>>;
}

/// Persists small per-session JSON values keyed by tool-chosen keys.
///
/// This is the seam for per-session tool state (todo lists, reminders,
/// scratchpads, ...): tools stay stateless and access their state through
/// [`ToolContext::session_state`](crate::types::ToolContext), while the
/// backing store decides where the values live (in memory, SQLite, ...).
///
/// Values are JSON-encoded strings; the typed accessors on
/// [`SessionState`](crate::types::SessionState) handle (de)serialization, so
/// tool state types must implement `serde::Serialize`/`Deserialize`.
#[async_trait]
pub trait SessionStateStore: Send + Sync {
    /// Loads the value stored for `key` in `session_id`, if any.
    async fn load(&self, session_id: &str, key: &str) -> anyhow::Result<Option<String>>;

    /// Stores `value` for `key` in `session_id`, replacing any previous value.
    async fn save(&self, session_id: &str, key: &str, value: String) -> anyhow::Result<()>;
}
