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
mod reminder;
pub use reminder::Reminder;
mod execution;
mod experiences;
mod media;
mod tools;

pub use compaction::CompactionError;
pub use experiences::SearchExperiencesTool;
pub use tools::execute_tool_with_panic_recovery;

use crate::middleware::TruncationService;
use crate::traits::{Experiences, LlmProvider, Memory, RequestLog, Tool, ToolRegistry};
use crate::types::{
    AgentEvent, ContentPart, Message, PathEnforcement, ProviderEvent, TokenUsage, ToolCall,
    ToolContext, ToolResult,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use std::any::Any;
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, trace, warn};
use uuid::Uuid;

use self::execution::{DoomLoopDetector, LlmResponseAggregator, execute_tool_call};
use crate::agent::media::extract_media_parts;

/// Maximum number of tool calls from a single LLM turn that execute at the
/// same time. Extra calls queue behind the semaphore until a slot frees up.
const MAX_PARALLEL_TOOL_CALLS: usize = 8;

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
    /// File-path confinement level passed to tools on every [`ToolContext`].
    enforcement: PathEnforcement,
    /// Optional experiences store for recording and retrieving past experiences.
    #[allow(dead_code)]
    experiences: Option<Arc<dyn Experiences>>,
    /// Optional request log; when set, every LLM request is recorded by the
    /// provider's `stream_response` (see [`crate::traits::RequestLog`]).
    log: Option<RequestLog>,
    /// Provider-reported token usage accumulated across the whole session
    /// (every completed LLM request, including compaction requests).
    provider_usage: TokenUsage,
    reminder: Reminder,
    /// Shared with every tool context of the active run.
    run_abort: Arc<AtomicBool>,
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
#[derive(Clone)]
pub struct CancelHandle(Arc<AtomicBool>);

impl Default for CancelHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelHandle {
    /// Create an independent handle. Used by callers that own a process or
    /// workspace lifetime outside an `Agent` (for example a disposable shell).
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
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
    reminder: Reminder,
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
    /// File-path confinement level passed to tools (defaults to none).
    enforcement: PathEnforcement,
    /// Session id surfaced to tools via [`ToolContext`]; defaults to a fresh
    /// UUID.
    session_id: Option<String>,
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
            reminder: Reminder::default(),
            max_turns: None,
            memory: None,
            context_limit: 256_000,
            experiences: None,
            log: None,
            enforcement: PathEnforcement::default(),
            session_id: None,
        }
    }
}

impl Agent {
    /// Change behavior while retaining memory and all other runtime state.
    /// The caller must ensure the agent is idle.
    pub fn set_profile(&mut self, system_prompt: String, tools: Vec<Arc<dyn Tool>>, reminder: Reminder) {
        self.system_prompt = system_prompt;
        self.tools = Arc::new(tools);
        self.reminder = reminder;
    }

    /// Swap the LLM provider while retaining memory and all other runtime
    /// state. The caller must ensure the agent is idle.
    pub fn set_llm(&mut self, llm: Arc<dyn LlmProvider>) {
        self.llm = llm;
    }

    /// The current mode reminder state (for persistence).
    /// A handle that cancels the currently streaming run.
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle(self.run_abort.clone())
    }

    pub fn reminder(&self) -> &Reminder {
        &self.reminder
    }

    /// Replace the mode reminder state (restored from persistence).
    pub fn set_reminder(&mut self, reminder: Reminder) {
        self.reminder = reminder;
    }
    /// Creates a new `AgentBuilder` for constructing `Agent` instances.
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
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
    /// 6. **Tool execution**: Execute all requested tool calls concurrently and add each result to history
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
        self.ask_with(user_message, RunOptions::default())
    }

    /// Runs one turn with explicit budgets and cancellation.
    pub fn run_with<'a>(
        &'a mut self,
        user_message: &'a str,
        options: RunOptions,
    ) -> impl Stream<Item = Result<AgentEvent>> + 'a {
        self.ask_with(user_message, options)
    }

    fn ask_with<'a>(
        &'a mut self,
        user_message: &'a str,
        options: RunOptions,
    ) -> impl Stream<Item = Result<AgentEvent>> + 'a {
        async_stream::try_stream! {
            if self.memory.messages().is_empty() && !self.system_prompt.is_empty() {
                self.memory.add_message(Message::System { content: self.system_prompt.clone() }).await;
            }

            self.memory.add_message(Message::User { content: vec![ContentPart::Text { text: user_message.to_string() }] }).await;
            self.reminder.on_user_turn();
            // Due mode reminders become part of conversation history, so
            // every request in this turn — and later turns — still carries
            // them instead of only the first provider call.
            self.reminder.apply(self.memory.messages_mut());

            self.run_abort.store(false, Ordering::SeqCst);
            let abort = self.run_abort.clone();
            let run_id = Uuid::new_v4().to_string();
            let max_turns = options.max_turns.or(self.max_turns);
            let max_tool_calls = options.max_tool_calls;
            let deadline = options.deadline.map(|limit| std::time::Instant::now() + limit);
            let mut tool_calls_made = 0usize;

            let mut turn = 0;
            let mut doom_loop = DoomLoopDetector::new(3);

            // Provider retry backoff is reported through this channel and
            // surfaced as `AgentEvent::Status`, so a rate-limit wait shows
            // up in the UI instead of looking like a silent hang.
            let (status_tx, mut status_rx) = mpsc::unbounded_channel::<String>();
            self.llm.set_status_sink(Some(Arc::new(move |message| {
                let _ = status_tx.send(message);
            })));

            loop {
                if abort.load(Ordering::SeqCst) {
                    yield AgentEvent::Cancelled { reason: "cancelled by owner".to_string() };
                    break;
                }
                if deadline.is_some_and(|limit| std::time::Instant::now() >= limit) {
                    yield AgentEvent::Error { error: "Run deadline exceeded.".to_string() };
                    break;
                }
                let context_tokens = crate::compaction::active_context(self.memory.messages())
                    .estimate_tokens();
                let compacting = crate::compaction::is_overflow(context_tokens, self.context_limit);
                if compacting {
                    yield AgentEvent::Status { message: "Compacting context…".into() };
                }
                if self.compact_with_estimate(context_tokens).await? {
                    info!("History compacted.");
                    let summary = match self.memory.messages().last() {
                        Some(Message::Compaction { summary, .. }) => summary.clone(),
                        _ => String::new(),
                    };
                    yield AgentEvent::Compacted { summary };
                } else if compacting {
                    // A skipped attempt must not leave the busy indicator stale.
                    yield self.usage_event();
                }

                if let Some(max) = max_turns
                    && turn >= max {
                        yield AgentEvent::Error { error: "Max turns reached without final answer.".to_string() };
                        break;
                    }
                turn += 1;

                let mut aggregator = LlmResponseAggregator::new();
                let mut messages = crate::compaction::active_context(self.memory.messages()).to_messages();
                // Persisted history may belong to a different agent profile.
                // Replace its base system prompt for this provider request,
                // without rewriting conversation history.
                if !self.system_prompt.is_empty() {
                    if let Some(Message::System { content }) = messages.first_mut() {
                        *content = self.system_prompt.clone();
                    } else {
                        messages.insert(0, Message::System { content: self.system_prompt.clone() });
                    }
                }
                trace!("LLM request messages: {:?}", messages);
                let tools_clone = self.tools.clone();

                // Interleave provider status messages (retry backoff) with
                // the awaited stream setup. The channel closes only when the
                // sink is replaced or the ask ends; `status_open` prevents a
                // closed channel from busy-looping the select.
                let mut status_open = true;
                let stream_fut = self
                    .llm
                    .stream_response(&messages, tools_clone.as_slice(), self.log.as_ref());
                tokio::pin!(stream_fut);
                let stream_result: Option<Result<_, _>> = loop {
                    tokio::select! {
                        result = &mut stream_fut => break Some(result),
                        status = status_rx.recv(), if status_open => {
                            match status {
                                Some(message) => {
                                    yield AgentEvent::Status { message };
                                }
                                None => status_open = false,
                            }
                        }
                    }
                };
                let mut stream = stream_result
                    .expect("select loop only exits by breaking with a result")
                    .map_err(|e| anyhow::anyhow!(e))?;

                // The provider's terminal stop reason (e.g. "max_tokens").
                // The aggregator drops it, so it is captured here before the
                // event is passed on.
                let mut finish_reason: Option<String> = None;
                while let Some(event_result) = stream.next().await {
                    let event = event_result.map_err(|e| anyhow::anyhow!(e))?;
                    match &event {
                        ProviderEvent::Finish { reason } => finish_reason = reason.clone(),
                        ProviderEvent::Usage { usage } => {
                            self.provider_usage.add(usage);
                        }
                        _ => {}
                    }
                    if let Some(agent_event) = aggregator.process_event(event) {
                        yield agent_event;
                    }
                }

                if abort.load(Ordering::SeqCst) {
                    yield AgentEvent::Cancelled { reason: "cancelled while streaming".to_string() };
                    break;
                }

                let (content, reasoning, tool_calls, replay) = aggregator.take_parts();

                // Reasoning models spend thinking tokens from the same
                // `max_tokens` budget; when it runs out the turn can end with
                // no text and no tool calls, which looks like a silent hang
                // in the UI. Surface the truncation instead of hiding it.
                if matches!(
                    finish_reason.as_deref(),
                    Some("max_tokens") | Some("length")
                ) {
                    warn!("LLM turn ended truncated (stop_reason: max_tokens)");
                    yield AgentEvent::Error {
                        error: "the model hit its output token limit (max_tokens); this reply \
                                may be truncated or empty — raise `max_tokens` in the config file"
                            .to_string(),
                    };
                }

                self.memory.add_message(Message::Assistant {
                    replay,
                    content: if content.is_empty() { None } else { Some(content.clone()) },
                    reasoning: if reasoning.is_empty() { None } else { Some(reasoning) },
                    tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls.clone()) },
                }).await;

                if tool_calls.is_empty() {
                    // Report the running context estimate at the turn boundary
                    // (frontends update the usage gauge from these events).
                    yield self.usage_event();
                    yield AgentEvent::Finish { response: content };
                    break;
                }

                info!("Agent requested {} tool calls", tool_calls.len());

                // Results keyed by the original call index. Slots are filled
                // by spawned tasks (and, for rejected calls, by synthesized
                // error results) so that EVERY tool call of the turn ends up
                // with a tool message in history — providers reject
                // assistant turns whose tool_calls were never answered.
                let mut results: Vec<Option<anyhow::Result<ToolResult>>> =
                    (0..tool_calls.len()).map(|_| None).collect();

                // Validate and announce every call first, then execute them
                // all concurrently. Doom-loop bookkeeping stays sequential so
                // the detector state matches the old per-call behavior.
                let mut pending: Vec<(usize, ToolCall, String)> = Vec::new();
                // Calls of this turn already announced with `ToolStart`. If
                // the doom-loop check aborts the turn, each of them must
                // still receive a `ToolFinish`, or the UI shows a tool card
                // that never completes.
                let mut announced: Vec<ToolCall> = Vec::new();
                for (index, call) in tool_calls.iter().enumerate() {
                    let tool_name = call.function.name.clone();
                    let args = call.function.arguments.clone();

                    if abort.load(Ordering::SeqCst) {
                        yield AgentEvent::Cancelled { reason: "cancelled between tool calls".to_string() };
                        return;
                    }
                    if let Some(max) = max_tool_calls {
                        if tool_calls_made >= max {
                            yield AgentEvent::Error {
                                error: format!("Tool call budget of {max} exhausted."),
                            };
                            return;
                        }
                    }
                    tool_calls_made += 1;

                    if let Some(error) = doom_loop.check(&tool_name, &args) {
                        yield AgentEvent::Error { error };
                        // Close out every announced call of this turn so the
                        // UI has no eternally "running" tool cards.
                        let not_executed = "Error: not executed because a doom loop was detected \
                                            and the run was stopped."
                            .to_string();
                        for announced_call in announced.drain(..) {
                            yield AgentEvent::ToolFinish {
                                call_id: announced_call.id.clone(),
                                tool: announced_call.function.name.clone(),
                                title: announced_call.function.name.clone(),
                                output: not_executed.clone(),
                                metadata: Default::default(),
                            };
                        }
                        // Close out every tool call of this turn so the
                        // persisted history has no dangling tool_calls.
                        for call in &tool_calls {
                            self.memory.add_message(Message::Tool {
                                tool_call_id: call.id.clone(),
                                tool_name: Some(call.function.name.clone()),
                                content: not_executed.clone(),
                                media: Vec::new(),
                            })
                            .await;
                        }
                        return;
                    }
                    doom_loop.record(tool_name.clone(), args.clone());

                    if tool_name.is_empty() {
                        yield AgentEvent::Error { error: "Tool name is empty, skipping execution".to_string() };
                        results[index] = Some(Ok(ToolResult::new(
                            tool_name.clone(),
                            "Error: the tool call had an empty name and was not executed.",
                        )));
                        continue;
                    }

                    yield AgentEvent::ToolStart { call_id: call.id.clone(), tool: tool_name.clone(), input: args.clone() };
                    announced.push(call.clone());
                    pending.push((index, call.clone(), Uuid::new_v4().to_string()));
                }

                // Execute every pending call in parallel, bounded by a
                // semaphore so a turn with many calls cannot exhaust system
                // resources.
                let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_PARALLEL_TOOL_CALLS));
                let mut join_set = tokio::task::JoinSet::new();
                for (index, call, message_id) in pending {
                    let tool = self
                        .tools
                        .iter()
                        .find(|t| t.name() == call.function.name)
                        .cloned();
                    let session_id = self.session_id.clone();
                    let run_id = run_id.clone();
                    let abort = abort.clone();
                    let enforcement = self.enforcement;
                    let truncation = self.truncation.clone();
                    let semaphore = semaphore.clone();
                    join_set.spawn(async move {
                        let _permit = semaphore.acquire_owned().await.ok();
                        let tool_name = call.function.name.clone();
                        let ctx = ToolContext::with_call(
                            &session_id,
                            &run_id,
                            &message_id,
                            &call.id,
                            abort,
                        )
                        .with_enforcement(enforcement);
                        let result = if let Some(t) = tool {
                            execute_tool_call(t, call, ctx, &truncation).await
                        } else {
                            Ok(ToolResult::new(
                                tool_name.clone(),
                                format!("Tool not found: {}", tool_name),
                            ))
                        };
                        (index, result)
                    });
                }

                // Collect results keyed by the original call index.
                while let Some(joined) = join_set.join_next().await {
                    if let Ok((index, result)) = joined {
                        results[index] = Some(result);
                    }
                }

                // Apply the results in the original call order so the
                // conversation history stays deterministic regardless of
                // completion order.
                for (call, result) in tool_calls.iter().zip(results) {
                    let tool_name = call.function.name.clone();
                    let result = match result {
                        Some(Ok(result)) => result,
                        Some(Err(e)) => {
                            yield AgentEvent::Error { error: e.to_string() };
                            ToolResult::new(tool_name.clone(), format!("Error: {}", e))
                        }
                        // The call's task panicked or was lost; still answer
                        // the tool_call so history stays consistent.
                        None => ToolResult::new(
                            tool_name.clone(),
                            "Error: the tool call did not produce a result (execution task failed).",
                        ),
                    };

                    yield AgentEvent::ToolFinish {
                        call_id: call.id.clone(),
                        tool: tool_name.clone(),
                        title: result.title.clone(),
                        output: result.output.clone(),
                        metadata: result.metadata.clone(),
                    };

                    let media_parts = extract_media_parts(&result.output);
                    // The media payload is forwarded via `media` parts; keep
                    // only a short text reference in the tool-message content
                    // so the base64 blob is not sent to the provider twice
                    // (and not counted twice by the token estimator).
                    let content = if media_parts.is_empty() {
                        result.output
                    } else {
                        match media_parts.iter().find(|p| {
                            matches!(p, ContentPart::Text { .. })
                        }) {
                            Some(ContentPart::Text { text }) => text.clone(),
                            _ => "[media attached]".to_string(),
                        }
                    };
                    self.memory.add_message(Message::Tool {
                        tool_call_id: call.id.clone(),
                        tool_name: Some(tool_name),
                        content,
                        media: media_parts,
                    }).await;
                }

                if abort.load(Ordering::SeqCst) {
                    yield AgentEvent::Cancelled { reason: "cancelled between tool calls".to_string() };
                    break;
                }
                // Report usage AFTER the tool results are in history: the
                // next LLM request will carry them, and emitting before would
                // under-report the context by the tool output size.
                yield self.usage_event();
            }
        }
    }

    /// Builds the context-usage event from the current conversation estimate
    /// plus the provider-reported token accounting for the whole session.
    fn usage_event(&self) -> AgentEvent {
        let used_tokens: usize = crate::compaction::active_context(self.memory.messages())
            .estimate_tokens();
        AgentEvent::Usage {
            used_tokens: used_tokens as u64,
            context_limit: self.context_limit as u64,
            provider: Some(self.provider_usage),
        }
    }

    /// Provider-reported token usage accumulated across the whole session
    /// (every completed LLM request, including compaction requests). All
    /// zeros until the provider reports usage.
    pub fn provider_usage(&self) -> TokenUsage {
        self.provider_usage
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

    /// Configure mode reminders for a newly created agent.
    pub fn with_reminder(mut self, reminder: Reminder) -> Self {
        self.reminder = reminder;
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

    /// Sets the file-path confinement level handed to tools on every
    /// [`ToolContext`]. Defaults to [`PathEnforcement::None`] (no checks).
    pub fn with_enforcement(mut self, enforcement: PathEnforcement) -> Self {
        self.enforcement = enforcement;
        self
    }

    /// Sets the session id surfaced to tools via [`ToolContext`].
    ///
    /// When the agent backs a durable conversation, this should be the same
    /// id the memory store is keyed by, so tools observe the real
    /// conversation identity. Defaults to a fresh UUID.
    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
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
            session_id: self
                .session_id
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            experiences: self.experiences,
            log: self.log,
            enforcement: self.enforcement,
            provider_usage: TokenUsage::default(),
            reminder: self.reminder,
            run_abort: Arc::new(AtomicBool::new(false)),
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
        /// When set, emitted through the registered status sink on every
        /// `send`, simulating provider-side retry reporting.
        status_message: Option<String>,
        status_sink: std::sync::Mutex<Option<crate::traits::StatusSink>>,
        /// When set, the stream yields one event then a mid-stream error.
        fail_stream: bool,
        /// Every request `build` produced, for wire-shape assertions.
        captured_requests: std::sync::Mutex<Vec<Value>>,
    }

    impl MockProvider {
        fn new(responses: Vec<Vec<ProviderEvent>>) -> Self {
            Self {
                responses,
                call: AtomicUsize::new(0),
                status_message: None,
                status_sink: std::sync::Mutex::new(None),
                fail_stream: false,
                captured_requests: std::sync::Mutex::new(Vec::new()),
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

        /// Content response preceded by a status message sent through the
        /// registered sink (as a retrying provider would report).
        fn content_with_status(delta: &str, status: &str) -> Self {
            let mut provider = Self::single_content(delta);
            provider.status_message = Some(status.to_string());
            provider
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        fn build(
            &self,
            messages: &[Message],
            tools: &[Arc<dyn Tool>],
        ) -> Result<Value, Box<dyn Error + Send + Sync>> {
            // Record the full built request so tests can assert on what the
            // wire would have carried.
            self.captured_requests
                .lock()
                .unwrap()
                .push(serde_json::json!({
                    "model": "mock",
                    "messages": messages,
                    "tools": tools.len(),
                }));
            Ok(serde_json::json!({
                "model": "mock",
                "messages": messages.len(),
                "tools": tools.len(),
            }))
        }

        fn set_status_sink(&self, sink: Option<crate::traits::StatusSink>) {
            *self.status_sink.lock().unwrap() = sink;
        }

        async fn send<'a>(
            &'a self,
            _request: Value,
        ) -> Result<
            BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
            Box<dyn Error + Send + Sync>,
        > {
            if let Some(message) = &self.status_message {
                // Snapshot the sink (no guard across await) and report it
                // while the send is still pending, as a real provider does
                // during backoff waits: the status is sent, then the future
                // stays pending, so the agent's select observes it.
                let sink = self.status_sink.lock().unwrap().clone();
                if let Some(sink) = sink {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    sink(message.clone());
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
            let index = self.call.fetch_add(1, Ordering::SeqCst);
            if self.fail_stream {
                // One event, then a mid-stream failure (e.g. a dropped
                // connection): exercises the response log's error path.
                let events: Vec<Result<ProviderEvent, Box<dyn Error + Send + Sync>>> = vec![
                    Ok(ProviderEvent::ContentDelta {
                        delta: "partial".to_string(),
                    }),
                    Err("stream dropped".into()),
                ];
                return Ok(Box::pin(futures::stream::iter(events)));
            }
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
    async fn reminders_persist_in_history_across_requests() -> Result<()> {
        let response = vec![ProviderEvent::ContentDelta { delta: "ok".into() }, ProviderEvent::Finish { reason: None }];
        let provider = Arc::new(MockProvider::new(vec![response; 25]));
        let mut agent = Agent::builder()
            .llm(provider.clone())
            .with_reminder(Reminder::switch("FULL MODE REMINDER", Some("SPARSE MODE REMINDER".into())))
            .build()?;
        for turn in 1..=25 {
            for event in agent.ask("hello").collect::<Vec<_>>().await {
                event?;
            }
            let requests = provider.captured_requests.lock().unwrap();
            let request = serde_json::to_string(requests.last().unwrap())?;
            // Reminders live in history: the switch reminder from turn 1 is
            // visible in every later request, and due heartbeats accumulate.
            let fulls = request.matches("FULL MODE REMINDER").count();
            let sparses = request.matches("SPARSE MODE REMINDER").count();
            assert_eq!(fulls, 1 + usize::from(turn % 25 == 0), "turn {turn}");
            assert_eq!(sparses, turn / 5 - turn / 25, "turn {turn}");
        }
        // And they are part of stored memory, not just the wire request.
        let memory = serde_json::to_string(agent.memory.messages())?;
        assert!(memory.contains("FULL MODE REMINDER"));
        assert!(memory.contains("SPARSE MODE REMINDER"));
        agent.set_profile(String::new(), Vec::new(), Reminder::default());
        Ok(())
    }

    /// The plan-mode regression: a reminder applied to history must still be
    /// visible in later LLM rounds of the same turn, not only the first.
    #[tokio::test]
    async fn reminder_survives_tool_rounds_within_one_turn() -> Result<()> {
        let tool_call = vec![
            ProviderEvent::ToolCallStart {
                id: "call_1".to_string(),
                name: "missing_tool".to_string(),
            },
            ProviderEvent::ToolCallDelta {
                id: "call_1".to_string(),
                delta: "{}".to_string(),
            },
            ProviderEvent::Finish {
                reason: Some("tool_calls".to_string()),
            },
        ];
        let final_response = vec![
            ProviderEvent::ContentDelta { delta: "done".into() },
            ProviderEvent::Finish { reason: None },
        ];
        let provider = Arc::new(MockProvider::new(vec![tool_call, final_response]));
        let mut agent = Agent::builder()
            .llm(provider.clone())
            .with_reminder(Reminder::switch("FULL MODE REMINDER", None))
            .build()?;
        for event in agent.ask("hello").collect::<Vec<_>>().await {
            event?;
        }
        let requests = provider.captured_requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "one tool round plus the final response");
        for (round, request) in requests.iter().enumerate() {
            let request = serde_json::to_string(request)?;
            assert!(
                request.contains("FULL MODE REMINDER"),
                "round {round} lost the reminder"
            );
        }
        Ok(())
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
        assert!(
            events
                .iter()
                .any(|e| matches!(e.as_ref().unwrap(), AgentEvent::Finish { .. }))
        );

        let entries = parse_log_entries(&buffer.contents());
        assert_eq!(entries.len(), 2, "request line + response line");
        assert_eq!(entries[0]["request"]["model"], "mock");
        assert!(entries[0]["timestamp"].is_string());
        let messages = entries[0]["request"]["messages"].as_u64().unwrap();
        assert_eq!(messages, 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_provider_status_surfaces_as_status_event() -> Result<()> {
        let mut agent = Agent::builder()
            .llm(Arc::new(MockProvider::content_with_status(
                "hi",
                "LLM request failed: rate limited; retrying in 4s (attempt 1)",
            )))
            .build()?;

        let events = agent.ask("hello").collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()));

        let status = events.iter().find_map(|e| match e.as_ref().unwrap() {
            AgentEvent::Status { message } => Some(message.clone()),
            _ => None,
        });
        assert_eq!(
            status.as_deref(),
            Some("LLM request failed: rate limited; retrying in 4s (attempt 1)"),
            "provider status must surface as an AgentEvent::Status"
        );
        // The status precedes the streamed content (it is emitted during
        // stream setup, before the response stream is consumed).
        let status_pos = events
            .iter()
            .position(|e| matches!(e.as_ref().unwrap(), AgentEvent::Status { .. }))
            .unwrap();
        let content_pos = events
            .iter()
            .position(|e| matches!(e.as_ref().unwrap(), AgentEvent::ContentDelta { .. }))
            .unwrap();
        assert!(status_pos < content_pos);
        Ok(())
    }

    #[tokio::test]
    async fn profile_switch_after_compaction_uses_current_prompt_and_reminder() -> Result<()> {
        let response = vec![ProviderEvent::ContentDelta { delta: "summary or answer".into() },
            ProviderEvent::Finish { reason: Some("stop".into()) }];
        let provider = Arc::new(MockProvider::new(vec![response; 3]));
        let mut agent = Agent::builder().llm(provider.clone()).context_limit(20_100)
            .system_prompt("old profile").build()?;
        let original = Message::User { content: vec![ContentPart::Text { text: "x".repeat(40_400) }] };
        agent.memory.add_message(original.clone()).await;
        {
            let events = agent.ask("continue");
            futures::pin_mut!(events);
            while let Some(event) = events.next().await { event?; }
        }
        assert!(agent.memory.messages().iter().any(|m| matches!(m, Message::Compaction { .. })));
        let stored = agent.memory.messages().to_vec();
        agent.set_profile("new profile".into(), vec![], Reminder::switch("NEW MODE REMINDER", None));
        {
            let events = agent.ask("next step");
            futures::pin_mut!(events);
            while let Some(event) = events.next().await { event?; }
        }
        assert_eq!(&agent.memory.messages()[..stored.len()], stored);
        assert!(agent.memory.messages().contains(&original));
        let requests = provider.captured_requests.lock().unwrap();
        assert_eq!(requests.len(), 3, "one summary, two normal requests");
        let final_request = &requests[2]["messages"];
        assert_eq!(final_request[0]["content"], "new profile");
        let serialized = serde_json::to_string(final_request)?;
        assert!(serialized.contains("NEW MODE REMINDER"));
        assert!(serialized.contains("<conversation-checkpoint>"));
        assert!(!serialized.contains("xxxx"));
        Ok(())
    }

    #[tokio::test]
    async fn repeated_compaction_preserves_transcript_and_folds_prior_context() -> Result<()> {
        let summary = || vec![ProviderEvent::ContentDelta { delta: "summary".into() }, ProviderEvent::Finish { reason: None }];
        let provider = Arc::new(MockProvider::new(vec![summary(), summary()]));
        let mut agent = Agent::builder().llm(provider.clone()).context_limit(20_100).build()?;
        let old = Message::User { content: vec![ContentPart::Text { text: "x".repeat(40_400) }] };
        agent.memory.add_message(old.clone()).await;
        agent.memory.add_message(Message::User { content: vec![ContentPart::Text { text: "prior tail".into() }] }).await;
        assert!(agent.compact_if_needed().await?);
        assert!(!agent.compact_if_needed().await?, "archived history must not trigger another compaction");
        agent.memory.add_message(old.clone()).await;
        assert!(agent.compact_if_needed().await?);
        assert_eq!(agent.memory.messages().len(), 5);
        assert_eq!(agent.memory.messages()[0], old);
        let requests = provider.captured_requests.lock().unwrap();
        let second = serde_json::to_string(&requests[1])?;
        assert!(second.contains("<prior-summary>"));
        assert!(second.contains("prior tail"));
        let context = crate::compaction::active_context(agent.memory.messages()).to_messages();
        assert_eq!(context.len(), 1);
        assert!(!serde_json::to_string(&context)?.contains("xxxx"));
        Ok(())
    }

    #[tokio::test]
    async fn empty_summary_does_not_append_compaction() -> Result<()> {
        let mut agent = Agent::builder().llm(Arc::new(MockProvider::single_content("")))
            .context_limit(20_100).build()?;
        let old = Message::User { content: vec![ContentPart::Text { text: "x".repeat(40_400) }] };
        agent.memory.add_message(old.clone()).await;
        assert!(agent.compact_if_needed().await.is_err());
        assert_eq!(agent.memory.messages(), &[old]);
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

        // The reserved buffer is clamped to half the window, so
        // usable = 20_100 - 10_050 = 10_050 tokens; the history below is
        // ~10_150 tokens and overflows. The last message fits the kept tail
        // (8k tokens), so the big first message becomes the summarized head.
        agent
            .memory
            .add_message(Message::User {
                content: vec![ContentPart::Text {
                    text: "x".repeat(40_400),
                }],
            })
            .await;
        agent
            .memory
            .add_message(Message::User {
                content: vec![ContentPart::Text {
                    text: "continue".to_string(),
                }],
            })
            .await;

        let compacted = agent.compact_if_needed().await?;
        assert!(compacted);

        let entries = parse_log_entries(&buffer.contents());
        assert_eq!(
            entries.len(),
            2,
            "compaction logs its request and response too"
        );
        assert_eq!(entries[0]["request"]["model"], "mock");
        assert_eq!(entries[1]["response"]["outcome"], "ok");

        // Stored originals survive; only model context collapses.
        assert_eq!(agent.memory.messages().len(), 3, "originals plus checkpoint");
        let messages = crate::compaction::active_context(agent.memory.messages()).to_messages();
        assert_eq!(messages.len(), 1, "model receives only the checkpoint");
        let text = match &messages[0] {
            Message::User { content } => match &content[0] {
                ContentPart::Text { text } => text.clone(),
                _ => panic!("expected text checkpoint"),
            },
            _ => panic!("expected user checkpoint message"),
        };
        assert!(text.contains("<conversation-checkpoint>"), "{text}");
        assert!(text.contains("summary"), "{text}");
        assert!(text.contains("[User]: continue"), "{text}");
        assert!(
            !text.contains("xxxx"),
            "the summarized head must not appear in the checkpoint"
        );
        Ok(())
    }

    #[tokio::test]
    async fn compaction_truncates_tool_outputs_in_the_summary_view() -> Result<()> {
        let provider = Arc::new(MockProvider::single_content("summary"));
        let mut agent = Agent::builder()
            .llm(provider.clone())
            .context_limit(20_100)
            .build()?;

        // A huge tool output in old history: the summarizer must see it
        // truncated, not verbatim.
        agent
            .memory
            .add_message(Message::Tool {
                tool_call_id: "c1".into(),
                tool_name: Some("read_file".into()),
                content: "y".repeat(80_000),
                media: vec![],
            })
            .await;
        agent
            .memory
            .add_message(Message::User {
                content: vec![ContentPart::Text {
                    text: "continue".to_string(),
                }],
            })
            .await;

        assert!(agent.compact_if_needed().await?);

        // The summary request's transcript carries the truncated form.
        let requests = provider.captured_requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "one summary request");
        let serialized = serde_json::to_string(&requests[0])?;
        assert!(
            serialized.contains("[Tool result]:"),
            "summary input uses the transcript format: {serialized}"
        );
        assert!(
            serialized.contains("[truncated]"),
            "summary input must mark truncation"
        );
        let shown = serialized.matches("yyyy").count();
        assert!(
            shown < 3_000,
            "summary input must cap tool output at ~2k chars, saw {shown} y's"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_with_log_records_response_timing_and_usage() -> Result<()> {
        let buffer = SharedBuffer::default();
        let mut agent = Agent::builder()
            .llm(Arc::new(MockProvider::new(vec![vec![
                ProviderEvent::ContentDelta {
                    delta: "hi".to_string(),
                },
                ProviderEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: 100,
                        cached_tokens: 700,
                        cache_write_tokens: 0,
                        output_tokens: 20,
                        reasoning_tokens: 5,
                    },
                },
                ProviderEvent::Finish { reason: None },
            ]])))
            .with_log(buffer.handle())
            .build()?;

        let events = agent.ask("hello").collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()));

        let entries = parse_log_entries(&buffer.contents());
        assert_eq!(entries.len(), 2, "one request line + one response line");
        let id = entries[0]["id"].as_str().unwrap();
        assert_eq!(entries[1]["id"].as_str().unwrap(), id, "correlated by id");

        let response = &entries[1]["response"];
        assert_eq!(response["outcome"], "ok");
        assert!(response["ttft_ms"].is_u64());
        assert!(response["total_ms"].is_u64());
        assert_eq!(response["events"], 3);
        let usage = &response["usage"];
        assert_eq!(usage["cached_tokens"], 700);
        assert_eq!(usage["reasoning_tokens"], 5);
        assert_eq!(usage["output_tokens"], 20);
        Ok(())
    }

    #[tokio::test]
    async fn test_with_log_records_stream_error_outcome() -> Result<()> {
        let buffer = SharedBuffer::default();
        let mut provider = MockProvider::new(Vec::new());
        provider.fail_stream = true;
        let mut agent = Agent::builder()
            .llm(Arc::new(provider))
            .with_log(buffer.handle())
            .build()?;

        let events = agent.ask("hello").collect::<Vec<_>>().await;
        assert!(events.iter().any(|e| e.is_err()), "stream error surfaces");

        let entries = parse_log_entries(&buffer.contents());
        assert_eq!(entries.len(), 2);
        let response = &entries[1]["response"];
        assert_eq!(response["outcome"], "error");
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .contains("stream dropped")
        );
        assert_eq!(response["events"], 1, "the event before the error");
        assert!(response["ttft_ms"].is_u64());
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
        assert!(
            events
                .iter()
                .any(|e| matches!(e.as_ref().unwrap(), AgentEvent::Finish { .. }))
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_without_log_agent_still_works() -> Result<()> {
        let mut agent = Agent::builder()
            .llm(Arc::new(MockProvider::single_content("hi")))
            .build()?;

        let events = agent.ask("hello").collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()));
        assert!(
            events
                .iter()
                .any(|e| matches!(e.as_ref().unwrap(), AgentEvent::Finish { .. }))
        );
        Ok(())
    }

    /// A reasoning model that exhausts the output budget on thinking ends the
    /// turn with no content and `stop_reason: "max_tokens"`. The agent must
    /// surface a truncation warning instead of finishing silently with an
    /// empty response.
    #[tokio::test]
    async fn test_max_tokens_finish_surfaces_a_truncation_warning() -> Result<()> {
        let provider = MockProvider::new(vec![vec![
            ProviderEvent::ReasoningDelta {
                delta: "thinking...".to_string(),
            },
            ProviderEvent::Finish {
                reason: Some("max_tokens".to_string()),
            },
        ]]);
        let mut agent = Agent::builder().llm(Arc::new(provider)).build()?;

        let events = agent.ask("hello").collect::<Vec<_>>().await;
        let events: Vec<AgentEvent> = events
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("stream yields no errors");

        let warning_position = events
            .iter()
            .position(|e| matches!(e, AgentEvent::Error { error } if error.contains("max_tokens")))
            .expect("a truncation warning must be yielded");
        let finish_position = events
            .iter()
            .position(|e| matches!(e, AgentEvent::Finish { response } if response.is_empty()))
            .expect("the ask still finishes (with an empty response)");
        assert!(
            warning_position < finish_position,
            "the warning must precede the terminal finish"
        );
        Ok(())
    }

    /// A normal completion (`stop_reason: "end_turn"`) must not trigger the
    /// truncation warning.
    #[tokio::test]
    async fn test_normal_finish_does_not_warn() -> Result<()> {
        let provider = MockProvider::new(vec![vec![
            ProviderEvent::ContentDelta {
                delta: "pong".to_string(),
            },
            ProviderEvent::Finish {
                reason: Some("end_turn".to_string()),
            },
        ]]);
        let mut agent = Agent::builder().llm(Arc::new(provider)).build()?;

        let events = agent.ask("hello").collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()));
        assert!(!events.iter().any(|e| matches!(
            e.as_ref().unwrap(),
            AgentEvent::Error { error } if error.contains("max_tokens")
        )));
        Ok(())
    }

    /// The detector must fire ON the threshold-th consecutive identical call,
    /// not one call after it (docs: "3 identical calls stops execution").
    #[test]
    fn test_doom_loop_fires_on_threshold_call() {
        let mut detector = super::execution::DoomLoopDetector::new(3);
        assert!(
            detector.check("read_file", r#"{"path":"a"}"#).is_none(),
            "first call must not trigger"
        );
        detector.record("read_file".into(), r#"{"path":"a"}"#.into());
        assert!(
            detector.check("read_file", r#"{"path":"a"}"#).is_none(),
            "second call must not trigger"
        );
        detector.record("read_file".into(), r#"{"path":"a"}"#.into());
        assert!(
            detector
                .check("read_file", r#"{"path":"a"}"#)
                .is_some(),
            "third identical call must trigger"
        );
    }

    /// An interrupted streak (different call in between) must not count
    /// toward the threshold.
    #[test]
    fn test_doom_loop_requires_consecutive_calls() {
        let mut detector = super::execution::DoomLoopDetector::new(3);
        for _ in 0..3 {
            detector.record("a_tool".into(), "{}".into());
            detector.record("b_tool".into(), "{}".into());
        }
        assert!(detector.check("a_tool", "{}").is_none());
    }

    /// A doom loop must stop the run on the third identical call and close
    /// out every announced tool call with a ToolFinish, so the UI never shows
    /// a tool card that never completes.
    #[tokio::test]
    async fn test_doom_loop_stops_run_and_finishes_announced_calls() -> Result<()> {
        let tool_call_response = || {
            vec![
                ProviderEvent::ToolCallStart {
                    id: "call_1".to_string(),
                    name: "missing_tool".to_string(),
                },
                ProviderEvent::ToolCallDelta {
                    id: "call_1".to_string(),
                    delta: "{}".to_string(),
                },
                ProviderEvent::Finish {
                    reason: Some("tool_calls".to_string()),
                },
            ]
        };
        // More copies than the threshold: without the fix the run would keep
        // looping; with it, it stops on the third identical call.
        let provider = MockProvider::new(vec![
            tool_call_response(),
            tool_call_response(),
            tool_call_response(),
            tool_call_response(),
            tool_call_response(),
        ]);
        let mut agent = Agent::builder().llm(Arc::new(provider)).build()?;

        let events = agent.ask("loop please").collect::<Vec<_>>().await;
        let events: Vec<&AgentEvent> = events.iter().map(|e| e.as_ref().unwrap()).collect();

        assert!(
            events.iter().any(
                |e| matches!(e, AgentEvent::Error { error } if error.contains("Doom loop detected"))
            ),
            "doom loop error must be surfaced: {events:?}"
        );

        let starts = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolStart { .. }))
            .count();
        let finishes = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolFinish { .. }))
            .count();
        assert_eq!(
            starts, finishes,
            "every announced ToolStart must receive a ToolFinish"
        );
        // The third identical call triggers the detector before it is
        // announced, so exactly threshold - 1 starts happen.
        assert_eq!(starts, 2);
        Ok(())
    }
}
