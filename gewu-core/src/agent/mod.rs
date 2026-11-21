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
mod media;
mod policy;
mod steering;
mod tools;

pub use compaction::CompactionError;
pub use media::extract_media_parts;
pub use policy::ToolPolicy;
pub use steering::{BackgroundJob, BackgroundNotification, IdleAction, InboxMessage, Steering};
pub use tools::execute_tool_with_panic_recovery;

use crate::middleware::TruncationService;
use crate::traits::{LlmProvider, Memory, RequestLog, Tool, ToolRegistry};
use crate::types::{
    AgentEvent, ContentPart, Message, PathEnforcement, ProgressSink, ProviderEvent, SessionState,
    TokenUsage, ToolCall, ToolContext, ToolResult,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use std::any::Any;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, trace, warn};
use uuid::Uuid;

use self::execution::{
    DoomLoopDetector, LlmResponseAggregator, ToolCallPlan, TurnDisposition, classify_turn,
    execute_tool_call, normalize_tool_result, plan_tool_call, prepare_provider_messages,
};

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
    /// Optional request log; when set, every LLM request is recorded by the
    /// provider's `stream_response` (see [`crate::traits::RequestLog`]).
    log: Option<RequestLog>,
    /// Provider-reported token usage accumulated across the whole session
    /// (every completed LLM request, including compaction requests).
    provider_usage: TokenUsage,
    reminder: Reminder,
    /// Tools that stay advertised but must not execute in this mode.
    tool_policy: ToolPolicy,
    /// Per-session key/value state shared with tools on every [`ToolContext`].
    session_state: SessionState,
    /// Session-selected filesystem root, when supplied by the host.
    working_directory: Option<PathBuf>,
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
    /// Advertised tools that must not execute (defaults to allow-all).
    tool_policy: ToolPolicy,
    /// Maximum turns limit (optional).
    max_turns: Option<usize>,
    /// Memory store (defaults to in-memory Vec).
    memory: Option<Box<dyn Memory>>,
    /// Context limit in tokens (defaults to 128,000).
    context_limit: usize,
    /// Request log for recording LLM requests (optional).
    log: Option<RequestLog>,
    /// File-path confinement level passed to tools (defaults to none).
    enforcement: PathEnforcement,
    /// Per-session tool state (defaults to a fresh in-memory store).
    session_state: Option<SessionState>,
    /// Session id surfaced to tools via [`ToolContext`]; defaults to a fresh
    /// UUID.
    session_id: Option<String>,
    /// Filesystem root handed to path-sensitive tools.
    working_directory: Option<PathBuf>,
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
            tool_policy: ToolPolicy::allow_all(),
            max_turns: None,
            memory: None,
            context_limit: 256_000,
            log: None,
            enforcement: PathEnforcement::default(),
            session_state: None,
            session_id: None,
            working_directory: None,
        }
    }
}

impl Agent {
    /// Change behavior while retaining memory and all other runtime state.
    /// The caller must ensure the agent is idle.
    pub fn set_profile(
        &mut self,
        system_prompt: String,
        tools: Vec<Arc<dyn Tool>>,
        reminder: Reminder,
        tool_policy: ToolPolicy,
    ) {
        self.system_prompt = system_prompt;
        self.tools = Arc::new(tools);
        self.reminder = reminder;
        self.tool_policy = tool_policy;
    }

    /// Swap the LLM provider while retaining memory and all other runtime
    /// state. The caller must ensure the agent is idle.
    pub fn set_llm(&mut self, llm: Arc<dyn LlmProvider>) {
        self.llm = llm;
    }

    /// Swap the catalog at a turn boundary without resetting its prompt or reminder.
    pub fn replace_tools(&mut self, tools: Arc<Vec<Arc<dyn Tool>>>) -> Arc<Vec<Arc<dyn Tool>>> {
        std::mem::replace(&mut self.tools, tools)
    }

    /// Changes the filesystem root used by subsequent tool calls.
    /// The caller must ensure the agent is idle.
    pub fn set_working_directory(&mut self, working_directory: impl Into<PathBuf>) {
        self.working_directory = Some(working_directory.into());
    }

    /// The current mode reminder state (for persistence).
    pub fn reminder(&self) -> &Reminder {
        &self.reminder
    }

    /// Replace the mode reminder state (restored from persistence).
    pub fn set_reminder(&mut self, reminder: Reminder) {
        self.reminder = reminder;
    }

    /// The execution policy applied to the advertised tools.
    pub fn tool_policy(&self) -> &ToolPolicy {
        &self.tool_policy
    }

    /// Replace the tool execution policy. The caller must ensure the agent
    /// is idle.
    pub fn set_tool_policy(&mut self, tool_policy: ToolPolicy) {
        self.tool_policy = tool_policy;
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

    /// Processes a user message with a fresh inbox and returns a stream of agent events.
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
        self.ask_with_inbox(user_message, Steering::new())
    }

    pub fn ask_with_inbox<'a>(
        &'a mut self,
        user_message: &'a str,
        inbox: Steering,
    ) -> impl Stream<Item = Result<AgentEvent>> + 'a {
        self.ask_parts_with_inbox(user_message, Vec::new(), inbox)
    }

    /// Run an agent turn with a prompt plus attached media parts (inline
    /// images or PDFs). The parts join the prompt text in one user message;
    /// `media` may be non-empty while `user_message` is empty.
    pub fn ask_parts_with_inbox<'a>(
        &'a mut self,
        user_message: &'a str,
        media: Vec<ContentPart>,
        inbox: Steering,
    ) -> impl Stream<Item = Result<AgentEvent>> + 'a {
        self.run_parts(Some(user_message), media, inbox)
    }

    pub fn resume(&mut self) -> impl Stream<Item = Result<AgentEvent>> + '_ {
        self.resume_with_inbox(Steering::new())
    }

    pub fn resume_with_inbox(
        &mut self,
        inbox: Steering,
    ) -> impl Stream<Item = Result<AgentEvent>> + '_ {
        self.run(None, inbox)
    }

    fn run<'a>(
        &'a mut self,
        user_message: Option<&'a str>,
        inbox: Steering,
    ) -> impl Stream<Item = Result<AgentEvent>> + 'a {
        self.run_parts(user_message, Vec::new(), inbox)
    }

    fn run_parts<'a>(
        &'a mut self,
        user_message: Option<&'a str>,
        media: Vec<ContentPart>,
        inbox: Steering,
    ) -> impl Stream<Item = Result<AgentEvent>> + 'a {
        async_stream::try_stream! {
            if self.memory.messages().is_empty() && !self.system_prompt.is_empty() {
                self.memory.add_message(Message::System { content: self.system_prompt.clone() }).await;
            }

            // Messages steered while the agent was idle join the transcript
            // ahead of this run's own prompt, preserving send order.
            for event in self.ingest_steered(&inbox).await {
                yield event;
            }

            if user_message.is_some() || !media.is_empty() {
                self.reminder.on_user_turn();
                // A due mode reminder becomes part of the message before it is
                // stored, so it is durable and every request in this turn — and
                // later turns — still carries it instead of only the first
                // provider call.
                let mut content = Vec::with_capacity(media.len() + 1);
                if let Some(text) = user_message.filter(|text| !text.is_empty()) {
                    content.push(ContentPart::Text {
                        text: text.to_string(),
                    });
                }
                content.extend(media);
                let mut message = Message::User { content };
                self.reminder.apply_to(&mut message);
                self.memory.add_message(message).await;
            }

            let mut turn = 0;
            let mut doom_loop = DoomLoopDetector::new(3);

            // Provider retry backoff is reported through this channel and
            // surfaced as `AgentEvent::Retrying`, so a rate-limit wait shows
            // up in the UI instead of looking like a silent hang.
            let (status_tx, mut status_rx) =
                mpsc::unbounded_channel::<crate::traits::RetryNotice>();
            self.llm.set_status_sink(Some(Arc::new(move |notice| {
                let _ = status_tx.send(notice);
            })));

            loop {
                // Inject messages steered mid-run before the context
                // estimate so they count toward compaction. This point is
                // the only safe spot: all tool results of the previous
                // round are in history, so a user message never lands
                // between an assistant turn and its tool results.
                for event in self.ingest_steered(&inbox).await {
                    yield event;
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

                if let Some(max) = self.max_turns
                    && turn >= max {
                        yield AgentEvent::Error { error: "Max turns reached without final answer.".to_string() };
                        break;
                    }
                turn += 1;

                let mut aggregator = LlmResponseAggregator::new();
                let messages = prepare_provider_messages(self.memory.messages(), &self.system_prompt);
                trace!("LLM request messages: {:?}", messages);
                let tools_clone = self.tools.clone();

                // Interleave provider retry notices with the awaited stream
                // setup. The channel closes only when the sink is replaced
                // or the ask ends; `status_open` prevents a closed channel
                // from busy-looping the select.
                let mut status_open = true;
                let stream_fut = self
                    .llm
                    .stream_response(&messages, tools_clone.as_slice(), self.log.as_ref());
                tokio::pin!(stream_fut);
                let stream_result: Option<Result<_, _>> = loop {
                    tokio::select! {
                        result = &mut stream_fut => break Some(result),
                        notice = status_rx.recv(), if status_open => {
                            match notice {
                                Some(notice) => {
                                    yield AgentEvent::Retrying {
                                        kind: notice.kind,
                                        message: notice.message,
                                        attempt: notice.attempt,
                                        delay_secs: notice.delay.as_secs(),
                                    };
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
                // Retry backoff also fires while the stream is being polled:
                // the Responses/Anthropic streams are lazy (the HTTP request
                // runs on first poll), and mid-stream restarts sleep inside
                // the stream too. Keep draining the status channel alongside
                // the events, so a retry wait shows up in the UI instead of
                // looking like a silent hang. The loop tracks the stream
                // only: the channel outlives the ask (the sink is replaced
                // by the next ask), so it never signals closure here.
                let mut stream_open = true;
                while stream_open {
                    let event_result = tokio::select! {
                        event_result = stream.next(), if stream_open => match event_result {
                            Some(event_result) => event_result,
                            None => {
                                stream_open = false;
                                continue;
                            }
                        },
                        notice = status_rx.recv(), if status_open => {
                            match notice {
                                Some(notice) => {
                                    yield AgentEvent::Retrying {
                                        kind: notice.kind,
                                        message: notice.message,
                                        attempt: notice.attempt,
                                        delay_secs: notice.delay.as_secs(),
                                    };
                                    continue;
                                }
                                None => {
                                    status_open = false;
                                    continue;
                                }
                            }
                        }
                    };
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

                let aggregated = aggregator.take_parts();
                let content = aggregated.content;
                let reasoning = aggregated.reasoning;
                let tool_calls = aggregated.tool_calls;
                let replay = aggregated.replay;

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

                let turn_disposition = classify_turn(&tool_calls, inbox.is_pending());
                if turn_disposition != TurnDisposition::Tools {
                    // A steer arriving while the model streams its final
                    // answer must still be answered in this run: continue
                    // instead of finishing. `max_turns` still bounds this.
                    if turn_disposition == TurnDisposition::Steered {
                        continue;
                    }
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

                    let plan = plan_tool_call(call, &mut doom_loop);
                    if let ToolCallPlan::Stop(ref error) = plan {
                        yield AgentEvent::Error { error: error.clone() };
                        // Close out every announced call of this turn so the
                        // UI has no eternally "running" tool cards.
                        let not_executed = "Error: not executed because a doom loop was detected \
                                            and the run was stopped."
                            .to_string();
                        for announced_call in announced.drain(..) {
                            yield AgentEvent::ToolFinish {
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
                    if let ToolCallPlan::Reject(result) = plan {
                        yield AgentEvent::Error { error: "Tool name is empty, skipping execution".to_string() };
                        results[index] = Some(Ok(result));
                        continue;
                    }

                    yield AgentEvent::ToolStart { tool: tool_name.clone(), input: args.clone() };
                    announced.push(call.clone());
                    pending.push((index, call.clone(), Uuid::new_v4().to_string()));
                }

                // Execute every pending call in parallel, bounded by a
                // semaphore so a turn with many calls cannot exhaust system
                // resources.
                let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_PARALLEL_TOOL_CALLS));
                let mut join_set = tokio::task::JoinSet::new();
                // Long-running tools report live status on this channel;
                // the collection loop below interleaves the events into the
                // ask stream while the calls execute.
                let (progress_tx, mut progress_rx) =
                    tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
                let mut executing = 0usize;
                for (index, call, message_id) in pending {
                    // Advertised tools can be refused per mode; check before
                    // dispatching so the tool never runs.
                    let denied = self.tool_policy.check(&call.function.name).err();
                    let tool = self
                        .tools
                        .iter()
                        .find(|t| t.name() == call.function.name)
                        .cloned();
                    let session_id = self.session_id.clone();
                    let session_state = self.session_state.clone();
                    let enforcement = self.enforcement;
                    let truncation = self.truncation.clone();
                    let semaphore = semaphore.clone();
                    let inbox = inbox.clone();
                    let working_directory = self.working_directory.clone();
                    let progress_tx = progress_tx.clone();
                    executing += 1;
                    join_set.spawn(async move {
                        let _permit = semaphore.acquire_owned().await.ok();
                        let tool_name = call.function.name.clone();
                        let mut ctx = ToolContext::new(&session_id, &message_id)
                            .with_enforcement(enforcement)
                            .with_session_state(session_state);
                        ctx.inbox = Some(inbox);
                        ctx.progress = Some(ProgressSink::new(&tool_name, progress_tx));
                        if let Some(working_directory) = working_directory {
                            ctx = ctx.with_working_directory(working_directory);
                        }
                        let result = if let Some(reason) = denied {
                            Err(anyhow::anyhow!(reason))
                        } else if let Some(t) = tool {
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
                drop(progress_tx);

                // Collect results keyed by the original call index,
                // interleaving live progress events from the running tools.
                // Progress queued after the last tool completes is dropped:
                // it is transient UI state, superseded by the ToolFinish.
                while executing > 0 {
                    tokio::select! {
                        joined = join_set.join_next() => {
                            if let Some(Ok((index, result))) = joined {
                                results[index] = Some(result);
                                executing -= 1;
                            }
                        }
                        progress = progress_rx.recv() => {
                            if let Some(event) = progress {
                                yield event;
                            }
                        }
                    }
                }

                // A tool's final event (e.g. a resolved question) is queued
                // just before its task returns, and the select above may have
                // consumed the join result first. Drain what is left so the
                // terminal event is not lost with the channel.
                while let Ok(event) = progress_rx.try_recv() {
                    yield event;
                }

                // Apply the results in the original call order so the
                // conversation history stays deterministic regardless of
                // completion order.
                for (call, result) in tool_calls.iter().zip(results) {
                    let normalized = normalize_tool_result(call, result);
                    if let Some(error) = normalized.error { yield error; }
                    yield normalized.finish;
                    self.memory.add_message(normalized.message).await;
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
        let used_tokens: usize =
            crate::compaction::active_context(self.memory.messages()).estimate_tokens();
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

    /// The per-session state store backing this agent's tools.
    pub fn session_state(&self) -> &SessionState {
        &self.session_state
    }

    /// Drains queued steering messages into conversation history as user
    /// messages, applying the same reminder treatment as an ordinary prompt.
    async fn ingest_steered(&mut self, inbox: &Steering) -> Vec<AgentEvent> {
        let messages = inbox.drain_messages();
        let mut events = Vec::new();
        // Steered user messages advance the cadence once per drain; a due
        // reminder attaches to the last of them before it is stored.
        let last_user = messages
            .iter()
            .rposition(|message| matches!(message, InboxMessage::User(_)));
        if last_user.is_some() {
            self.reminder.on_user_turn();
        }
        for (index, message) in messages.into_iter().enumerate() {
            match message {
                InboxMessage::User(text) => {
                    let mut user = Message::User {
                        content: vec![ContentPart::Text { text: text.clone() }],
                    };
                    if Some(index) == last_user {
                        self.reminder.apply_to(&mut user);
                    }
                    self.memory.add_message(user).await;
                    events.push(AgentEvent::Steered { text });
                }
                InboxMessage::Background(notification) => {
                    let message = notification.text();
                    self.memory
                        .add_message(Message::User {
                            content: vec![ContentPart::Text {
                                text: message.clone(),
                            }],
                        })
                        .await;
                    events.push(AgentEvent::Status { message });
                }
            }
        }
        events
    }

    /// The tools this agent can call.
    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// Extracts an immutable template from this agent for spawning clones.
    ///
    /// The template captures everything that does not change per
    /// conversation — provider, tools, prompts, limits — so new agents can
    /// be created by supplying only the per-session pieces (memory, session
    /// id, reminder) via [`AgentTemplate::spawn`]. This avoids rebuilding
    /// tool registries when repeatedly spawning the same profile.
    pub fn template(&self) -> AgentTemplate {
        AgentTemplate {
            llm: Arc::clone(&self.llm),
            tools: Arc::clone(&self.tools),
            system_prompt: self.system_prompt.clone(),
            tool_policy: self.tool_policy.clone(),
            max_turns: self.max_turns,
            context_limit: self.context_limit,
            truncation: self.truncation.clone(),
            enforcement: self.enforcement,
            log: self.log.clone(),
            session_state: self.session_state.clone(),
            working_directory: self.working_directory.clone(),
        }
    }
}

/// Immutable, cloneable agent configuration extracted via
/// [`Agent::template`].
///
/// A template holds everything that is shared across conversations; the
/// per-session state (memory, session id, reminder, usage counter) is
/// supplied fresh on [`AgentTemplate::spawn`].
#[derive(Clone)]
pub struct AgentTemplate {
    llm: Arc<dyn LlmProvider>,
    tools: Arc<Vec<Arc<dyn Tool>>>,
    system_prompt: String,
    tool_policy: ToolPolicy,
    max_turns: Option<usize>,
    context_limit: usize,
    truncation: TruncationService,
    enforcement: PathEnforcement,
    log: Option<RequestLog>,
    session_state: SessionState,
    working_directory: Option<PathBuf>,
}

impl AgentTemplate {
    /// The tools spawned agents can call.
    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// The system prompt spawned agents run with.
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Replaces the LLM provider used by spawned agents.
    pub fn with_llm(mut self, llm: Arc<dyn LlmProvider>) -> Self {
        self.llm = llm;
        self
    }

    /// Replaces the tool set of spawned agents.
    pub fn with_tools(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.tools = Arc::new(tools);
        self
    }

    /// Replaces the system prompt of spawned agents.
    pub fn with_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = system_prompt.into();
        self
    }

    /// Replaces the tool execution policy of spawned agents.
    pub fn with_tool_policy(mut self, tool_policy: ToolPolicy) -> Self {
        self.tool_policy = tool_policy;
        self
    }

    /// The tool execution policy spawned agents run with.
    pub fn tool_policy(&self) -> &ToolPolicy {
        &self.tool_policy
    }

    /// Replaces the context limit of spawned agents.
    pub fn with_context_limit(mut self, context_limit: usize) -> Self {
        self.context_limit = context_limit;
        self
    }

    /// Replaces the per-session state store of spawned agents.
    pub fn with_session_state(mut self, session_state: SessionState) -> Self {
        self.session_state = session_state;
        self
    }

    /// Creates a new agent from this template with the given per-session
    /// state. The usage counter starts at zero and the reminder is supplied
    /// by the caller (fresh sessions pass [`Reminder::default`]).
    pub fn spawn(
        self,
        session_id: impl Into<String>,
        memory: Box<dyn Memory>,
        reminder: Reminder,
    ) -> Agent {
        Agent {
            llm: self.llm,
            tools: self.tools,
            system_prompt: self.system_prompt,
            tool_policy: self.tool_policy,
            max_turns: self.max_turns,
            memory,
            context_limit: self.context_limit,
            truncation: self.truncation,
            session_id: session_id.into(),
            enforcement: self.enforcement,
            log: self.log,
            session_state: self.session_state,
            provider_usage: TokenUsage::default(),
            reminder,
            working_directory: self.working_directory,
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
        self.tool_policy
            .check(name)
            .map_err(|reason| -> Box<dyn Error + Send + Sync> { reason.into() })?;
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

    /// Configure which advertised tools may not execute.
    ///
    /// Denied tools stay in the tool list — so request prefixes remain stable
    /// across modes — but calls to them fail with the policy's message.
    /// Defaults to [`ToolPolicy::allow_all`].
    pub fn with_tool_policy(mut self, tool_policy: ToolPolicy) -> Self {
        self.tool_policy = tool_policy;
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

    /// Sets the per-session state store handed to tools on every
    /// [`ToolContext`]. Defaults to a fresh in-memory store.
    pub fn with_session_state(mut self, session_state: SessionState) -> Self {
        self.session_state = Some(session_state);
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

    /// Sets the filesystem root handed to path-sensitive tools.
    pub fn working_directory(mut self, working_directory: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(working_directory.into());
        self
    }

    /// Builds the `Agent` instance.
    ///
    /// # Errors
    ///
    /// Returns an error if no LLM provider was set.
    pub fn build(self) -> Result<Agent> {
        let llm = self.llm.context("LLM provider is required")?;

        let tools = Arc::new((*self.tools).clone());

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
            log: self.log,
            enforcement: self.enforcement,
            provider_usage: TokenUsage::default(),
            reminder: self.reminder,
            tool_policy: self.tool_policy,
            session_state: self.session_state.unwrap_or_else(SessionState::in_memory),
            working_directory: self.working_directory,
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
        /// When set, the error text reported through the registered status
        /// sink on every `send`, simulating provider-side retry reporting.
        status_message: Option<String>,
        /// When set, the error text reported through the registered status
        /// sink by the returned *stream* before its first event, simulating
        /// a lazy request or a mid-stream restart reporting a retry wait.
        stream_status: Option<String>,
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
                stream_status: None,
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

        /// Content response preceded by a retry notice sent through the
        /// registered sink (as a retrying provider would report).
        fn content_with_status(delta: &str, error: &str) -> Self {
            let mut provider = Self::single_content(delta);
            provider.status_message = Some(error.to_string());
            provider
        }

        /// Content response whose *stream* reports a retry notice through
        /// the registered sink before its first event — how the lazy
        /// Responses/Anthropic streams surface a retrying request.
        fn content_with_stream_status(delta: &str, error: &str) -> Self {
            let mut provider = Self::single_content(delta);
            provider.stream_status = Some(error.to_string());
            provider
        }
    }

    /// The retry notice a mock provider reports: first attempt, fixed 4s
    /// wait, carrying `message` as the failure text.
    fn mock_notice(message: &str) -> crate::traits::RetryNotice {
        crate::traits::RetryNotice {
            kind: crate::types::RetryKind::Status(502),
            message: message.to_string(),
            attempt: 1,
            delay: std::time::Duration::from_secs(4),
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
                // during backoff waits: the notice is sent, then the future
                // stays pending, so the agent's select observes it.
                let sink = self.status_sink.lock().unwrap().clone();
                if let Some(sink) = sink {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    sink(mock_notice(message));
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
            let mut stream = futures::stream::iter(events.into_iter().map(Ok)).boxed();
            if let Some(message) = &self.stream_status {
                // Report the notice from inside the stream, as a provider
                // whose request/retry happens on first poll would: the sink
                // fires while the consumer waits for the first event.
                let sink = self.status_sink.lock().unwrap().clone();
                let message = message.clone();
                let prefix = futures::stream::once(async move {
                    if let Some(sink) = sink {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        sink(mock_notice(&message));
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                // The prefix only performs the sink call: it contributes
                // no events.
                .filter_map(|()| futures::future::ready(None));
                stream = prefix.chain(stream).boxed();
            }
            Ok(stream)
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

    /// Wraps a `Vec<Message>` and records each message at `add_message` time,
    /// so tests can assert what a persistence backend actually received.
    #[derive(Default)]
    struct RecordingMemory {
        messages: Vec<Message>,
        added: Arc<Mutex<Vec<Message>>>,
    }

    #[async_trait]
    impl Memory for RecordingMemory {
        fn messages(&self) -> &[Message] {
            &self.messages
        }

        async fn add_message(&mut self, message: Message) {
            self.added.lock().unwrap().push(message.clone());
            self.messages.push(message);
        }

        async fn clear(&mut self) {
            self.messages.clear();
        }

        async fn replace(&mut self, messages: Vec<Message>) {
            self.messages = messages;
        }
    }

    #[tokio::test]
    async fn reminders_are_applied_before_messages_are_stored() -> Result<()> {
        let added = Arc::new(Mutex::new(Vec::new()));
        let memory = RecordingMemory {
            messages: Vec::new(),
            added: added.clone(),
        };
        let mut agent = Agent::builder()
            .llm(Arc::new(MockProvider::single_content("ok")))
            .with_reminder(Reminder::switch("FULL MODE REMINDER", None))
            .memory(Box::new(memory))
            .build()?;
        for event in agent.ask("hello").collect::<Vec<_>>().await {
            event?;
        }
        let added = added.lock().unwrap();
        let user = added
            .iter()
            .find_map(|message| match message {
                Message::User { content } => Some(content),
                _ => None,
            })
            .expect("the user message must be stored");
        assert!(
            user.iter().any(|part| matches!(
                part,
                ContentPart::Reminder { text } if text == "FULL MODE REMINDER"
            )),
            "the backend stored an undecorated user message: {user:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn attachments_join_the_user_message() -> Result<()> {
        let added = Arc::new(Mutex::new(Vec::new()));
        let memory = RecordingMemory {
            messages: Vec::new(),
            added: added.clone(),
        };
        let mut agent = Agent::builder()
            .llm(Arc::new(MockProvider::single_content("ok")))
            .memory(Box::new(memory))
            .build()?;
        let media = vec![
            ContentPart::Image {
                data: "aGk=".into(),
                mime_type: "image/png".into(),
            },
            ContentPart::File {
                data: "aGk=".into(),
                mime_type: "application/pdf".into(),
            },
        ];
        for event in agent
            .ask_parts_with_inbox("describe this", media.clone(), Steering::new())
            .collect::<Vec<_>>()
            .await
        {
            event?;
        }
        let added = added.lock().unwrap();
        let user = added
            .iter()
            .find_map(|message| match message {
                Message::User { content } => Some(content),
                _ => None,
            })
            .expect("the user message must be stored");
        assert_eq!(
            user,
            &vec![
                ContentPart::Text {
                    text: "describe this".into()
                },
                media[0].clone(),
                media[1].clone(),
            ],
            "the prompt text must precede the media parts: {user:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn attachments_without_prompt_still_start_a_turn() -> Result<()> {
        let added = Arc::new(Mutex::new(Vec::new()));
        let memory = RecordingMemory {
            messages: Vec::new(),
            added: added.clone(),
        };
        let mut agent = Agent::builder()
            .llm(Arc::new(MockProvider::single_content("ok")))
            .memory(Box::new(memory))
            .build()?;
        for event in agent
            .ask_parts_with_inbox(
                "",
                vec![ContentPart::Image {
                    data: "aGk=".into(),
                    mime_type: "image/png".into(),
                }],
                Steering::new(),
            )
            .collect::<Vec<_>>()
            .await
        {
            event?;
        }
        let added = added.lock().unwrap();
        let user = added
            .iter()
            .find_map(|message| match message {
                Message::User { content } => Some(content),
                _ => None,
            })
            .expect("the user message must be stored");
        assert_eq!(user.len(), 1, "no empty text part is stored: {user:?}");
        assert!(matches!(&user[0], ContentPart::Image { .. }));
        Ok(())
    }

    #[tokio::test]
    async fn reminders_persist_in_history_across_requests() -> Result<()> {
        let response = vec![
            ProviderEvent::ContentDelta { delta: "ok".into() },
            ProviderEvent::Finish { reason: None },
        ];
        let provider = Arc::new(MockProvider::new(vec![response; 25]));
        let mut agent = Agent::builder()
            .llm(provider.clone())
            .with_reminder(Reminder::switch(
                "FULL MODE REMINDER",
                Some("SPARSE MODE REMINDER".into()),
            ))
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
        agent.set_profile(
            String::new(),
            Vec::new(),
            Reminder::default(),
            ToolPolicy::allow_all(),
        );
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
            ProviderEvent::ContentDelta {
                delta: "done".into(),
            },
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
    async fn test_provider_retry_surfaces_as_retrying_event() -> Result<()> {
        let mut agent = Agent::builder()
            .llm(Arc::new(MockProvider::content_with_status(
                "hi",
                "rate limited",
            )))
            .build()?;

        let events = agent.ask("hello").collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()));

        let retrying = events.iter().find_map(|e| match e.as_ref().unwrap() {
            AgentEvent::Retrying {
                kind,
                message,
                attempt,
                delay_secs,
            } => Some((kind.clone(), message.clone(), *attempt, *delay_secs)),
            _ => None,
        });
        assert_eq!(
            retrying,
            Some((
                crate::types::RetryKind::Status(502),
                "rate limited".to_string(),
                1,
                4
            )),
            "provider retry must surface as an AgentEvent::Retrying"
        );
        // The notice precedes the streamed content (it is emitted during
        // stream setup, before the response stream is consumed).
        let retrying_pos = events
            .iter()
            .position(|e| matches!(e.as_ref().unwrap(), AgentEvent::Retrying { .. }))
            .unwrap();
        let content_pos = events
            .iter()
            .position(|e| matches!(e.as_ref().unwrap(), AgentEvent::ContentDelta { .. }))
            .unwrap();
        assert!(retrying_pos < content_pos);
        Ok(())
    }

    #[tokio::test]
    async fn test_stream_phase_retry_surfaces_as_retrying_event() -> Result<()> {
        // The Responses/Anthropic streams are lazy: their HTTP request (and
        // any retry wait) runs while the event stream is polled, not while
        // `stream_response` is awaited. The notice must still surface.
        let mut agent = Agent::builder()
            .llm(Arc::new(MockProvider::content_with_stream_status(
                "hi",
                "502 Bad Gateway",
            )))
            .build()?;

        let events = agent.ask("hello").collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()));

        let retrying_pos = events
            .iter()
            .position(|e| {
                matches!(
                    e.as_ref().unwrap(),
                    AgentEvent::Retrying { message, .. } if message == "502 Bad Gateway"
                )
            })
            .expect("a stream-phase retry must surface as AgentEvent::Retrying");
        let content_pos = events
            .iter()
            .position(|e| matches!(e.as_ref().unwrap(), AgentEvent::ContentDelta { .. }))
            .unwrap();
        assert!(retrying_pos < content_pos);
        Ok(())
    }

    #[tokio::test]
    async fn profile_switch_after_compaction_uses_current_prompt_and_reminder() -> Result<()> {
        let response = vec![
            ProviderEvent::ContentDelta {
                delta: "summary or answer".into(),
            },
            ProviderEvent::Finish {
                reason: Some("stop".into()),
            },
        ];
        let provider = Arc::new(MockProvider::new(vec![response; 3]));
        let mut agent = Agent::builder()
            .llm(provider.clone())
            .context_limit(20_100)
            .system_prompt("old profile")
            .build()?;
        let original = Message::User {
            content: vec![ContentPart::Text {
                text: "x".repeat(40_400),
            }],
        };
        agent.memory.add_message(original.clone()).await;
        {
            let events = agent.ask("continue");
            futures::pin_mut!(events);
            while let Some(event) = events.next().await {
                event?;
            }
        }
        assert!(
            agent
                .memory
                .messages()
                .iter()
                .any(|m| matches!(m, Message::Compaction { .. }))
        );
        let stored = agent.memory.messages().to_vec();
        agent.set_profile(
            "new profile".into(),
            vec![],
            Reminder::switch("NEW MODE REMINDER", None),
            ToolPolicy::allow_all(),
        );
        {
            let events = agent.ask("next step");
            futures::pin_mut!(events);
            while let Some(event) = events.next().await {
                event?;
            }
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
        let summary = || {
            vec![
                ProviderEvent::ContentDelta {
                    delta: "summary".into(),
                },
                ProviderEvent::Finish { reason: None },
            ]
        };
        let provider = Arc::new(MockProvider::new(vec![summary(), summary()]));
        let mut agent = Agent::builder()
            .llm(provider.clone())
            .context_limit(20_100)
            .build()?;
        let old = Message::User {
            content: vec![ContentPart::Text {
                text: "x".repeat(40_400),
            }],
        };
        agent.memory.add_message(old.clone()).await;
        agent
            .memory
            .add_message(Message::User {
                content: vec![ContentPart::Text {
                    text: "prior tail".into(),
                }],
            })
            .await;
        assert!(agent.compact_if_needed().await?);
        assert!(
            !agent.compact_if_needed().await?,
            "archived history must not trigger another compaction"
        );
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
    async fn empty_summary_does_not_add_a_checkpoint() -> Result<()> {
        let mut agent = Agent::builder()
            .llm(Arc::new(MockProvider::single_content("")))
            .context_limit(20_100)
            .build()?;
        let old = Message::User {
            content: vec![ContentPart::Text {
                text: "x".repeat(40_400),
            }],
        };
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
        assert_eq!(
            agent.memory.messages().len(),
            3,
            "originals plus checkpoint"
        );
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
            detector.check("read_file", r#"{"path":"a"}"#).is_some(),
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

    /// A tool that appends a value to a per-session list in session state,
    /// for asserting that tool contexts carry the agent's session state.
    struct StateProbeTool;

    #[async_trait]
    impl Tool for StateProbeTool {
        fn name(&self) -> &str {
            "state_probe"
        }

        fn description(&self) -> &str {
            "appends to session state"
        }

        fn schema(&self) -> schemars::schema::RootSchema {
            schemars::schema_for!(serde_json::Value)
        }

        async fn execute(
            &self,
            args: Value,
            ctx: &ToolContext,
        ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
            let mut items: Vec<String> = ctx
                .session_state
                .read(&ctx.session_id, "probe")
                .await
                .map_err(|e| -> Box<dyn Error + Send + Sync> { e.to_string().into() })?
                .unwrap_or_default();
            items.push(args["value"].as_str().unwrap_or("").to_string());
            ctx.session_state
                .write(&ctx.session_id, "probe", &items)
                .await
                .map_err(|e| -> Box<dyn Error + Send + Sync> { e.to_string().into() })?;
            Ok(ToolResult::new("state_probe", format!("{items:?}")))
        }
    }

    /// Tool contexts must carry the agent's session state (so stateless
    /// tools persist per session), and templates must spawn clones that
    /// share that state handle while keeping per-session memory fresh.
    #[tokio::test]
    async fn tool_contexts_carry_session_state_across_spawns() -> Result<()> {
        let provider = MockProvider::new(vec![
            vec![
                ProviderEvent::ToolCallStart {
                    id: "call_1".to_string(),
                    name: "state_probe".to_string(),
                },
                ProviderEvent::ToolCallDelta {
                    id: "call_1".to_string(),
                    delta: r#"{"value":"one"}"#.to_string(),
                },
                ProviderEvent::Finish {
                    reason: Some("tool_calls".to_string()),
                },
            ],
            vec![
                ProviderEvent::ContentDelta {
                    delta: "done".to_string(),
                },
                ProviderEvent::Finish {
                    reason: Some("end_turn".to_string()),
                },
            ],
        ]);
        let mut agent = Agent::builder()
            .llm(Arc::new(provider))
            .with_tools(vec![Arc::new(StateProbeTool)])
            .session_id("sess-1")
            .build()?;

        let events = agent.ask("probe please").collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()), "{events:?}");

        // The tool wrote through the injected context.
        let items: Vec<String> = agent
            .session_state()
            .read("sess-1", "probe")
            .await?
            .expect("tool must have written session state");
        assert_eq!(items, vec!["one".to_string()]);

        // A spawn sharing the state handle and session id sees the state.
        let template = agent.template();
        let clone = template
            .clone()
            .spawn("sess-1", Box::new(Vec::new()), Reminder::default());
        let items: Vec<String> = clone
            .session_state()
            .read("sess-1", "probe")
            .await?
            .expect("spawned clone shares the state store");
        assert_eq!(items, vec!["one".to_string()]);

        // A spawn into a different session sees nothing.
        let fresh = template.spawn("sess-2", Box::new(Vec::new()), Reminder::default());
        assert_eq!(
            fresh
                .session_state()
                .read::<Vec<String>>("sess-2", "probe")
                .await?,
            None
        );
        Ok(())
    }

    /// A tool can stay advertised (stable request prefix) while the agent's
    /// policy refuses to execute it.
    #[tokio::test]
    async fn tool_policy_refuses_execution_without_hiding_the_tool() -> Result<()> {
        let provider = MockProvider::new(vec![
            vec![
                ProviderEvent::ToolCallStart {
                    id: "call_1".to_string(),
                    name: "state_probe".to_string(),
                },
                ProviderEvent::ToolCallDelta {
                    id: "call_1".to_string(),
                    delta: r#"{"value":"one"}"#.to_string(),
                },
                ProviderEvent::Finish {
                    reason: Some("tool_calls".to_string()),
                },
            ],
            vec![
                ProviderEvent::ContentDelta {
                    delta: "done".to_string(),
                },
                ProviderEvent::Finish {
                    reason: Some("end_turn".to_string()),
                },
            ],
        ]);
        let mut agent = Agent::builder()
            .llm(Arc::new(provider))
            .with_tools(vec![Arc::new(StateProbeTool)])
            .session_id("sess-1")
            .with_tool_policy(ToolPolicy::deny(["state_probe"], "read-only mode"))
            .build()?;

        assert!(agent.list_tools().iter().any(|name| name == "state_probe"));

        let ctx = ToolContext::new("sess-1", "msg");
        let error = ToolRegistry::execute(
            &agent,
            "state_probe",
            serde_json::json!({"value": "one"}),
            &ctx,
        )
        .await
        .expect_err("the policy must refuse the call");
        assert!(error.to_string().contains("read-only mode"), "{error}");

        let events = agent.ask("probe please").collect::<Vec<_>>().await;
        assert!(
            events.iter().any(|event| matches!(
                event,
                Ok(AgentEvent::Error { error }) if error.contains("read-only mode")
            )),
            "{events:?}"
        );
        assert!(
            agent.memory.messages().iter().any(|message| matches!(
                message,
                Message::Tool { content, .. } if content.contains("Error: read-only mode")
            )),
            "the model must see the refusal"
        );
        assert_eq!(
            agent
                .session_state()
                .read::<Vec<String>>("sess-1", "probe")
                .await?,
            None,
            "the refused tool must not run"
        );
        Ok(())
    }

    /// A long-running tool reporting through the context's progress sink
    /// must surface `ToolProgress` events on the ask stream, between the
    /// call's `ToolStart` and `ToolFinish`.
    struct ProgressProbeTool;

    #[async_trait]
    impl Tool for ProgressProbeTool {
        fn name(&self) -> &str {
            "progress_probe"
        }

        fn description(&self) -> &str {
            "reports progress"
        }

        fn schema(&self) -> schemars::schema::RootSchema {
            schemars::schema_for!(serde_json::Value)
        }

        async fn execute(
            &self,
            _args: Value,
            ctx: &ToolContext,
        ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
            ctx.report_progress("halfway there");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            ctx.report_progress("almost done");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(ToolResult::new("progress_probe", "ok"))
        }
    }

    #[tokio::test]
    async fn tool_progress_events_reach_the_ask_stream() -> Result<()> {
        let provider = MockProvider::new(vec![
            vec![
                ProviderEvent::ToolCallStart {
                    id: "call_1".to_string(),
                    name: "progress_probe".to_string(),
                },
                ProviderEvent::ToolCallDelta {
                    id: "call_1".to_string(),
                    delta: "{}".to_string(),
                },
                ProviderEvent::Finish {
                    reason: Some("tool_calls".to_string()),
                },
            ],
            vec![
                ProviderEvent::ContentDelta {
                    delta: "done".to_string(),
                },
                ProviderEvent::Finish {
                    reason: Some("end_turn".to_string()),
                },
            ],
        ]);
        let mut agent = Agent::builder()
            .llm(Arc::new(provider))
            .with_tools(vec![Arc::new(ProgressProbeTool)])
            .build()?;

        let events = agent.ask("probe").collect::<Vec<_>>().await;
        let events: Vec<&AgentEvent> = events.iter().map(|e| e.as_ref().unwrap()).collect();

        let start = events
            .iter()
            .position(
                |e| matches!(e, AgentEvent::ToolStart { tool, .. } if tool == "progress_probe"),
            )
            .expect("tool must start");
        let finish = events
            .iter()
            .position(
                |e| matches!(e, AgentEvent::ToolFinish { tool, .. } if tool == "progress_probe"),
            )
            .expect("tool must finish");
        let progress: Vec<&str> = events[start + 1..finish]
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolProgress { tool, message } if tool == "progress_probe" => {
                    Some(message.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(progress, vec!["halfway there", "almost done"]);
        Ok(())
    }
}
