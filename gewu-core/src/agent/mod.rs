//! Agent implementation for the GeWu framework.
//!
//! The [`Agent`] is the core runtime that orchestrates conversation flow,
//! tool execution, and context management. It follows a loop-based model
//! where it processes user input, calls the LLM, handles tool executions,
//! and continues until a final response is generated.
//!
//! # Agent Loop
//!
//! The [`ask`](Agent::ask) method implements the main agent loop:
//!
//! 1. **System prompt setup**: If no conversation history exists, add system prompt
//! 2. **Add user message**: Add the user's input to history
//! 3. **Compaction check**: If history exceeds token limit, summarize older messages
//! 4. **LLM request**: Send conversation to LLM with available tools
//! 5. **Stream response**: Yield content/reasoning deltas as they arrive
//! 6. **Tool execution**: For each tool call, execute the tool and add result to history
//! 7. **Repeat**: Continue from step 3 until LLM produces a final response
//!
//! # Doom Loop Detection
//!
//! To prevent infinite loops of identical tool calls, the agent tracks the
//! last 3 tool calls. If the same tool is called 3 times in a row with
//! identical arguments, execution is stopped with an error.
//!
//! # Example
//!
//! ```ignore
//! use gewu_core::Agent;
//! use gewu_providers::OpenAIProvider;
//!
//! let provider = OpenAIProvider::new(&api_key, "gpt-4");
//! let agent = Agent::builder()
//!     .llm(Arc::new(provider))
//!     .with_tools(vec![...])
//!     .system_prompt("You are a helpful assistant.")
//!     .build()?;
//! ```

mod compaction;
mod execution;
mod experiences;
mod media;
mod tools;

pub use compaction::CompactionError;
pub use experiences::SearchExperiencesTool;
pub use tools::execute_tool_with_panic_recovery;

use crate::middleware::TruncationService;
use crate::traits::{Experiences, LlmProvider, Memory, RequestLog, Tool, ToolRegistry};
use crate::types::{AgentEvent, ContentPart, Message, ToolContext, ToolResult};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use std::any::Any;
use std::error::Error;
use std::sync::Arc;
use tracing::{info, trace};
use uuid::Uuid;

use self::execution::{DoomLoopDetector, LlmResponseAggregator, execute_tool_call};
use crate::agent::media::extract_media_parts;
use crate::compaction::prune;

/// The main runtime that orchestrates conversation flow, tool execution, and context management.
///
/// The `Agent` follows a loop-based model where it processes user input, calls the LLM,
/// handles tool executions, and continues until a final response is generated.
///
/// Use [`Agent::builder()`] to create an `AgentBuilder` for constructing `Agent` instances.
pub struct Agent {
    /// The LLM provider used for generating responses.
    llm: Arc<dyn LlmProvider>,
    /// Available tools the agent can call.
    tools: Arc<Vec<Arc<dyn Tool>>>,
    /// System prompt used to configure agent behavior.
    system_prompt: String,
    /// Maximum number of conversation turns before forcing a final response.
    max_turns: Option<usize>,
    /// Memory store for conversation history.
    pub memory: Box<dyn Memory>,
    /// Maximum context window size in tokens.
    pub context_limit: usize,
    /// Service for truncating long tool outputs.
    truncation: TruncationService,
    /// Unique session identifier.
    session_id: String,
    /// Optional experiences store for recording and retrieving past experiences.
    #[allow(dead_code)]
    experiences: Option<Arc<dyn Experiences>>,
    /// Optional request log; when set, every LLM request is recorded by the
    /// provider's `stream_response` (see [`crate::traits::RequestLog`]).
    log: Option<RequestLog>,
    /// Shared with every tool context of the active run.
    run_abort: Arc<std::sync::atomic::AtomicBool>,
}

/// Per-run budgets. `Default` keeps the historical behaviour.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Overrides the agent's `max_turns` for this run.
    pub max_turns: Option<usize>,
    /// Hard cap on tool executions in one run.
    pub max_tool_calls: Option<usize>,
    /// Wall-clock budget for the whole run.
    pub deadline: Option<std::time::Duration>,
}

/// A handle that can cancel a run while its stream is being consumed.
///
/// `Agent::run_with` borrows the agent mutably, so the owner needs this handle
/// rather than `&Agent` to stop a run from another task.
///
/// Semantics: the handle cancels the run that is currently streaming. A cancel
/// with no active run has no effect — the next run clears the flag when it
/// starts, so a stale Stop cannot poison a later request.
#[derive(Clone)]
pub struct CancelHandle(Arc<std::sync::atomic::AtomicBool>);

impl CancelHandle {
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Builder for constructing `Agent` instances.
pub struct AgentBuilder {
    /// The LLM provider (required).
    llm: Option<Arc<dyn LlmProvider>>,
    /// Available tools (defaults to empty).
    tools: Arc<Vec<Arc<dyn Tool>>>,
    /// System prompt (defaults to empty).
    system_prompt: String,
    /// Maximum turns limit (optional).
    max_turns: Option<usize>,
    /// Memory store (defaults to in-memory Vec).
    memory: Option<Box<dyn Memory>>,
    /// Context limit in tokens (defaults to 128,000).
    context_limit: usize,
    /// Experiences store (optional).
    experiences: Option<Arc<dyn Experiences>>,
    /// Request log for recording LLM requests (optional).
    log: Option<RequestLog>,
}

/// Extracts a readable message from a panic payload.
///
/// This function handles different types of panic payloads and converts them
/// to human-readable strings for error reporting.
///
/// # Arguments
///
/// * `panic_payload` - The raw panic payload from a caught panic
///
/// # Returns
///
/// A string describing the panic cause
pub fn extract_panic_message(panic_payload: Box<dyn Any + Send>) -> String {
    if let Some(s) = panic_payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic_payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(e) = panic_payload.downcast_ref::<Box<dyn Error + Send + 'static>>() {
        e.to_string()
    } else {
        "Unknown panic cause".to_string()
    }
}

impl Default for AgentBuilder {
    fn default() -> Self {
        AgentBuilder {
            llm: None,
            tools: Arc::new(Vec::new()),
            system_prompt: String::new(),
            max_turns: None,
            memory: None,
            context_limit: 128_000,
            experiences: None,
            log: None,
        }
    }
}

impl Agent {
    /// Creates a new `AgentBuilder` for constructing `Agent` instances.
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// Handle that stops the current or next run.
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle(self.run_abort.clone())
    }

    /// Resets the agent to its initial state.
    ///
    /// Clears all conversation history and re-adds the system prompt if one was configured.
    pub async fn reset(&mut self) {
        self.memory.clear().await;
        if !self.system_prompt.is_empty() {
            self.memory
                .add_message(Message::System {
                    content: self.system_prompt.clone(),
                })
                .await;
        }
    }

    /// Processes a user message and returns a stream of agent events.
    ///
    /// This method implements the main agent loop:
    ///
    /// 1. **System prompt setup**: If no conversation history exists, add system prompt
    /// 2. **Add user message**: Add the user's input to history
    /// 3. **Compaction check**: If history exceeds token limit, summarize older messages
    /// 4. **LLM request**: Send conversation to LLM with available tools
    /// 5. **Stream response**: Yield content/reasoning deltas as they arrive
    /// 6. **Tool execution**: For each tool call, execute the tool and add result to history
    /// 7. **Repeat**: Continue from step 3 until LLM produces a final response
    ///
    /// # Arguments
    ///
    /// * `user_message` - The user's input message
    ///
    /// # Returns
    ///
    /// A stream of [`AgentEvent`] items representing the agent's progress and response
    ///
    /// # Doom Loop Detection
    ///
    /// If the same tool is called 3 times in a row with identical arguments,
    /// execution is stopped with an error to prevent infinite loops.
    pub fn ask<'a>(
        &'a mut self,
        user_message: &'a str,
    ) -> impl Stream<Item = Result<AgentEvent>> + 'a {
        self.run_with(user_message, RunOptions::default())
    }

    /// Runs one turn with explicit budgets and cancellation.
    ///
    /// The stream ends after exactly one terminal event: [`AgentEvent::Finish`]
    /// (success), [`AgentEvent::Cancelled`] (owner stopped it), or
    /// [`AgentEvent::Error`] (turn budget exhausted or the run failed).
    pub fn run_with<'a>(
        &'a mut self,
        user_message: &'a str,
        options: RunOptions,
    ) -> impl Stream<Item = Result<AgentEvent>> + 'a {
        async_stream::try_stream! {
            self.run_abort.store(false, std::sync::atomic::Ordering::SeqCst);
            let abort = self.run_abort.clone();
            let run_id = Uuid::new_v4().to_string();
            let max_turns = options.max_turns.or(self.max_turns);
            let max_tool_calls = options.max_tool_calls;
            let deadline = options.deadline.map(|limit| std::time::Instant::now() + limit);

            if self.memory.messages().is_empty() && !self.system_prompt.is_empty() {
                self.memory.add_message(Message::System { content: self.system_prompt.clone() }).await;
            }

            self.memory.add_message(Message::User { content: vec![ContentPart::Text { text: user_message.to_string() }] }).await;

            let mut turn = 0;
            let mut tool_calls_made = 0usize;
            let mut doom_loop = DoomLoopDetector::new(3);
            let mut message_id = Uuid::new_v4().to_string();

            loop {
                if abort.load(std::sync::atomic::Ordering::SeqCst) {
                    yield AgentEvent::Cancelled { reason: "cancelled by owner".to_string() };
                    break;
                }
                if deadline.is_some_and(|limit| std::time::Instant::now() >= limit) {
                    yield AgentEvent::Error { error: "Run deadline exceeded.".to_string() };
                    break;
                }

                if self.compact_if_needed().await? {
                    info!("History compacted.");
                }

                if let Some(max) = max_turns {
                    if turn >= max {
                        yield AgentEvent::Error { error: "Max turns reached without final answer.".to_string() };
                        break;
                    }
                }
                turn += 1;

                prune(self.memory.messages_mut());

                let mut aggregator = LlmResponseAggregator::new();
                let messages = self.memory.messages().to_vec();
                trace!("LLM request messages: {:?}", messages);

                let tools_clone = self.tools.clone();
                let mut stream = self
                    .llm
                    .stream_response(&messages, tools_clone.as_slice(), self.log.as_ref())
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;

                while let Some(event_result) = stream.next().await {
                    if abort.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    let event = event_result.map_err(|e| anyhow::anyhow!(e))?;
                    if let Some(agent_event) = aggregator.process_event(event) {
                        yield agent_event;
                    }
                }

                if abort.load(std::sync::atomic::Ordering::SeqCst) {
                    // Do not record a partial assistant turn: the provider may
                    // still be mid-stream, and a truncated tool call would leave
                    // history that cannot be answered.
                    yield AgentEvent::Cancelled { reason: "cancelled while streaming".to_string() };
                    break;
                }

                let (content, tool_calls) = aggregator.take_parts();

                self.memory.add_message(Message::Assistant {
                    content: if content.is_empty() { None } else { Some(content.clone()) },
                    tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls.clone()) },
                }).await;

                if tool_calls.is_empty() {
                    yield AgentEvent::Finish { response: content };
                    break;
                }

                info!("Agent requested {} tool calls", tool_calls.len());

                let mut cancelled = false;
                for call in tool_calls {
                    let tool_name = call.function.name.clone();
                    let args = call.function.arguments.clone();
                    let call_id = call.id.clone();

                    if abort.load(std::sync::atomic::Ordering::SeqCst) {
                        cancelled = true;
                        break;
                    }

                    if let Some(error) = doom_loop.check(&tool_name, &args) {
                        yield AgentEvent::Error { error };
                        return;
                    }
                    doom_loop.record(tool_name.clone(), args.clone());

                    if let Some(max) = max_tool_calls {
                        if tool_calls_made >= max {
                            yield AgentEvent::Error {
                                error: format!("Tool call budget of {max} exhausted."),
                            };
                            return;
                        }
                    }
                    tool_calls_made += 1;

                    if tool_name.is_empty() {
                        // Still answer the call: history must stay protocol-valid.
                        yield AgentEvent::Error {
                            error: "Tool name is empty, skipping execution".to_string(),
                        };
                        let output = "Error: tool name is empty".to_string();
                        yield AgentEvent::ToolStart {
                            call_id: call_id.clone(),
                            tool: tool_name.clone(),
                            input: args.clone(),
                        };
                        yield AgentEvent::ToolFinish {
                            call_id: call_id.clone(),
                            tool: tool_name.clone(),
                            title: String::new(),
                            output: output.clone(),
                            metadata: Default::default(),
                        };
                        self.memory.add_message(Message::Tool {
                            tool_call_id: call_id,
                            tool_name: Some(tool_name.clone()),
                            content: output,
                            media: Vec::new(),
                        }).await;
                        continue;
                    }

                    yield AgentEvent::ToolStart {
                        call_id: call_id.clone(),
                        tool: tool_name.clone(),
                        input: args.clone(),
                    };

                    let ctx = ToolContext::with_call(
                        &self.session_id,
                        &run_id,
                        &message_id,
                        &call_id,
                        abort.clone(),
                    );
                    message_id = Uuid::new_v4().to_string();

                    let tool = self.tools.iter().find(|t| t.name() == tool_name);

                    let result = if let Some(t) = tool {
                        match execute_tool_call(t.clone(), call, ctx, &self.truncation).await {
                            Ok(result) => result,
                            // A malformed argument payload is a tool outcome, not
                            // a reason to skip the Tool message: the provider
                            // rejects a run whose tool calls have no answers.
                            Err(e) => {
                                let output = format!("Error: {}", e);
                                yield AgentEvent::Error { error: output.clone() };
                                ToolResult::new(tool_name.clone(), output)
                            }
                        }
                    } else {
                        ToolResult::new(
                            tool_name.clone(),
                            format!("Tool not found: {}", tool_name),
                        )
                    };

                    yield AgentEvent::ToolFinish {
                        call_id: call_id.clone(),
                        tool: tool_name.clone(),
                        title: result.title.clone(),
                        output: result.output.clone(),
                        metadata: result.metadata.clone(),
                    };

                    let media_parts = extract_media_parts(&result.output);
                    self.memory.add_message(Message::Tool {
                        tool_call_id: call_id,
                        tool_name: Some(tool_name.clone()),
                        content: result.output,
                        media: media_parts,
                    }).await;
                }

                if cancelled {
                    yield AgentEvent::Cancelled { reason: "cancelled between tool calls".to_string() };
                    break;
                }
            }
        }
    }
}

#[async_trait]
impl ToolRegistry for Agent {
    async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let tool = self.tools.iter().find(|t| t.name() == name);
        match tool {
            Some(t) => t.execute(args, ctx).await,
            None => Err(format!("Tool not found: {}", name).into()),
        }
    }

    fn list_tools(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }
}

impl AgentBuilder {
    /// Sets the LLM provider for the agent.
    ///
    /// This is a required field - the builder will fail if no LLM provider is set.
    pub fn llm(mut self, llm: Arc<dyn LlmProvider>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// Sets the tools available to the agent.
    ///
    /// Tools are the functions the agent can call during conversation.
    pub fn with_tools(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.tools = Arc::new(tools);
        self
    }

    /// Sets the system prompt for the agent.
    ///
    /// The system prompt is used to configure agent behavior and is typically
    /// the first message in the conversation.
    pub fn system_prompt(mut self, prompt: &str) -> Self {
        self.system_prompt = prompt.to_string();
        self
    }

    /// Sets the maximum number of conversation turns.
    ///
    /// A turn consists of a user message, LLM response, and any tool executions.
    /// If set, the agent will force a final response after this many turns.
    pub fn max_turns(mut self, max_turns: Option<usize>) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Sets the context limit in tokens.
    ///
    /// This determines when the agent should compact or prune conversation history.
    /// Defaults to 128,000 tokens.
    pub fn context_limit(mut self, limit: usize) -> Self {
        self.context_limit = limit;
        self
    }

    /// Sets the memory store for conversation history.
    ///
    /// Defaults to an in-memory `Vec<Message>` if not specified.
    pub fn memory(mut self, memory: Box<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Sets the experiences store for experience search.
    ///
    /// When set, a [`search_experiences`](SearchExperiencesTool) tool is automatically
    /// added to the agent, allowing the LLM to recall relevant past experiences.
    ///
    /// If not set, the search_experiences tool will not be available.
    pub fn with_experiences(mut self, experiences: Arc<dyn Experiences>) -> Self {
        self.experiences = Some(experiences);
        self
    }

    /// Sets the request log used to record every LLM request.
    ///
    /// When set, each request sent to the LLM provider is recorded as one JSONL
    /// line: `{"timestamp": "...", "request": {...wire request...}}`. The log
    /// handle can be shared with other agents (`RequestLog` is an `Arc`).
    pub fn with_log(mut self, log: RequestLog) -> Self {
        self.log = Some(log);
        self
    }

    /// Builds the `Agent` instance.
    ///
    /// # Errors
    ///
    /// Returns an error if no LLM provider was set.
    pub fn build(self) -> Result<Agent> {
        let llm = self.llm.context("LLM provider is required")?;

        let mut tools = (*self.tools).clone();
        if let Some(ref experiences) = self.experiences {
            tools.push(Arc::new(SearchExperiencesTool::new(experiences.clone())));
        }
        let tools = Arc::new(tools);

        let agent = Agent {
            llm,
            tools,
            system_prompt: self.system_prompt.clone(),
            max_turns: self.max_turns,
            memory: self.memory.unwrap_or_else(|| Box::new(Vec::new())),
            context_limit: self.context_limit,
            truncation: TruncationService::new(),
            session_id: Uuid::new_v4().to_string(),
            experiences: self.experiences,
            log: self.log,
            run_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        Ok(agent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProviderEvent;
    use futures::stream::BoxStream;
    use serde_json::Value;
    use std::io::Write;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records written bytes in a shared buffer for assertions.
    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        /// Returns a shareable request-log handle over this buffer.
        fn handle(&self) -> RequestLog {
            Arc::new(Mutex::new(Box::new(self.clone())))
        }

        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A writer that always fails, for best-effort logging tests.
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("write failed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("flush failed"))
        }
    }

    /// Builds a canned wire request and streams canned responses.
    struct MockProvider {
        responses: Vec<Vec<ProviderEvent>>,
        call: AtomicUsize,
    }

    impl MockProvider {
        fn new(responses: Vec<Vec<ProviderEvent>>) -> Self {
            Self {
                responses,
                call: AtomicUsize::new(0),
            }
        }

        fn single_content(delta: &str) -> Self {
            Self::new(vec![vec![
                ProviderEvent::ContentDelta {
                    delta: delta.to_string(),
                },
                ProviderEvent::Finish { reason: None },
            ]])
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        fn build(
            &self,
            messages: &[Message],
            tools: &[Arc<dyn Tool>],
        ) -> Result<Value, Box<dyn Error + Send + Sync>> {
            Ok(serde_json::json!({
                "model": "mock",
                "messages": messages.len(),
                "tools": tools.len(),
            }))
        }

        async fn send<'a>(
            &'a self,
            _request: Value,
        ) -> Result<
            BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
            Box<dyn Error + Send + Sync>,
        > {
            let index = self.call.fetch_add(1, Ordering::SeqCst);
            let events = self.responses.get(index).cloned().unwrap_or_default();
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }

        async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
            Ok(Vec::new())
        }
    }

    fn parse_log_entries(log: &str) -> Vec<serde_json::Value> {
        log.lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).expect("log line must be valid JSON"))
            .collect()
    }

    #[tokio::test]
    async fn test_with_log_records_turn_requests() -> Result<()> {
        let buffer = SharedBuffer::default();
        let mut agent = Agent::builder()
            .llm(Arc::new(MockProvider::single_content("hi")))
            .with_log(buffer.handle())
            .build()?;

        let events = agent.ask("hello world").collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()));
        assert!(events
            .iter()
            .any(|e| matches!(e.as_ref().unwrap(), AgentEvent::Finish { .. })));

        let entries = parse_log_entries(&buffer.contents());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["request"]["model"], "mock");
        assert!(entries[0]["timestamp"].is_string());
        let messages = entries[0]["request"]["messages"].as_u64().unwrap();
        assert_eq!(messages, 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_with_log_records_compaction_requests() -> Result<()> {
        let buffer = SharedBuffer::default();
        let mut agent = Agent::builder()
            .llm(Arc::new(MockProvider::single_content("summary")))
            .context_limit(20_100)
            .with_log(buffer.handle())
            .build()?;

        // usable = 20_100 - 20_000 buffer = 100 tokens; ~125 tokens overflows.
        agent
            .memory
            .add_message(Message::User {
                content: vec![ContentPart::Text {
                    text: "x".repeat(500),
                }],
            })
            .await;

        let compacted = agent.compact_if_needed().await?;
        assert!(compacted);

        let entries = parse_log_entries(&buffer.contents());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["request"]["model"], "mock");
        Ok(())
    }

    #[tokio::test]
    async fn test_log_write_failure_does_not_break_the_turn() -> Result<()> {
        let log: RequestLog = Arc::new(Mutex::new(Box::new(FailingWriter)));
        let mut agent = Agent::builder()
            .llm(Arc::new(MockProvider::single_content("hi")))
            .with_log(log)
            .build()?;

        let events = agent.ask("hello").collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()));
        assert!(events
            .iter()
            .any(|e| matches!(e.as_ref().unwrap(), AgentEvent::Finish { .. })));
        Ok(())
    }

    #[tokio::test]
    async fn test_without_log_agent_still_works() -> Result<()> {
        let mut agent = Agent::builder()
            .llm(Arc::new(MockProvider::single_content("hi")))
            .build()?;

        let events = agent.ask("hello").collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()));
        assert!(events
            .iter()
            .any(|e| matches!(e.as_ref().unwrap(), AgentEvent::Finish { .. })));
        Ok(())
    }
}
