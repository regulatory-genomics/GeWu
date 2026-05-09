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

use crate::compaction::{self, estimate_message_tokens, is_overflow};
use crate::compaction::{
    ConsolidationInput, apply_consolidation_to_memory, build_consolidation_prompt,
    build_consolidation_text, save_consolidation_file,
};
use crate::middleware::TruncationService;
use crate::permission::{
    PermissionDecision, PermissionDecisionKind, PermissionGrant, ToolPermissionRequest,
};
use crate::traits::{LlmProvider, Memory, Tool, ToolRegistry};
use crate::types::{
    AgentEvent, ContentPart, FunctionCall, Message, PermissionBroker, ProviderEvent, ToolCall,
    ToolContext, ToolExecutionPolicy, ToolResult,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde::Deserialize;
use std::any::Any;
use std::collections::VecDeque;
use std::error::Error;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use tokio::task::spawn_blocking;
use tracing::{debug, info, warn};
use uuid::Uuid;

fn strip_tag_blocks(input: &str, tag: &str) -> String {
    let start_tag = format!("<{tag}>");
    let end_tag = format!("</{tag}>");
    let mut output = input.to_string();

    loop {
        let lower = output.to_lowercase();
        let Some(start) = lower.find(&start_tag) else {
            break;
        };
        let Some(end_rel) = lower[start..].find(&end_tag) else {
            break;
        };
        let end = start + end_rel + end_tag.len();
        output.replace_range(start..end, "");
    }

    output
}

fn normalize_assistant_output(input: &str) -> String {
    let cleaned = ["think", "analysis", "reasoning"]
        .into_iter()
        .fold(input.to_string(), |acc, tag| strip_tag_blocks(&acc, tag));
    cleaned.trim().to_string()
}

fn is_max_output_stop(reason: Option<&str>) -> bool {
    let Some(reason) = reason else {
        return false;
    };
    matches!(
        reason.trim().to_ascii_lowercase().as_str(),
        "max_tokens" | "max_output_tokens" | "length" | "max_tokens_stop" | "max-tokens"
    )
}

pub struct Agent {
    llm: Arc<dyn LlmProvider>,
    tools: Arc<Vec<Arc<dyn Tool>>>,
    system_prompt: String,
    max_turns: Option<usize>,
    pub memory: Box<dyn Memory>,
    pub context_limit: usize,
    truncation: TruncationService,
    session_id: String,
    tool_context_metadata: serde_json::Map<String, serde_json::Value>,
    permission_broker: Option<Arc<dyn PermissionBroker>>,
}

pub struct AgentBuilder {
    llm: Option<Arc<dyn LlmProvider>>,
    tools: Arc<Vec<Arc<dyn Tool>>>,
    system_prompt: String,
    max_turns: Option<usize>,
    memory: Option<Box<dyn Memory>>,
    context_limit: usize,
    session_id: Option<String>,
    tool_context_metadata: serde_json::Map<String, serde_json::Value>,
    permission_broker: Option<Arc<dyn PermissionBroker>>,
}

fn permission_request_from_output(output: &str) -> Option<ToolPermissionRequest> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    if value
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(serde_json::Value::as_str)
        != Some("permission_required")
    {
        return None;
    }

    value
        .get("permission_request")
        .cloned()
        .and_then(|request| serde_json::from_value(request).ok())
}

fn permission_grant_from_decision(
    request: &ToolPermissionRequest,
    decision: &PermissionDecision,
) -> Option<PermissionGrant> {
    if decision.request_id != request.id || decision.decision != PermissionDecisionKind::Approve {
        return None;
    }

    Some(PermissionGrant {
        session_id: request.session_id.clone(),
        action: request.action.clone(),
        scope: request.scope.clone(),
        duration: decision.duration,
    })
}

fn add_permission_grant_to_context(ctx: &mut ToolContext, grant: PermissionGrant) {
    let mut grants = ctx.permission_grants();
    grants.push(grant);
    if let Ok(value) = serde_json::to_value(grants) {
        ctx.metadata.insert("permission_grants".to_string(), value);
    }
}

fn extract_panic_message(panic_payload: Box<dyn Any + Send>) -> String {
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

async fn execute_tool_with_panic_recovery(
    tool: Arc<dyn Tool>,
    args: serde_json::Value,
    ctx: ToolContext,
) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
    let tool_name = tool.name().to_string();

    let result = spawn_blocking(move || {
        catch_unwind(AssertUnwindSafe(|| {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(tool.execute(args, &ctx))
        }))
    })
    .await;

    match result {
        Ok(Ok(res)) => res,
        Ok(Err(e)) => {
            let panic_msg = extract_panic_message(e);
            Err(Box::new(std::io::Error::other(format!(
                "Tool '{tool_name}' panicked during execution: {panic_msg}. This may indicate an issue with \
                    the tool implementation or invalid input. Consider adjusting the tool arguments \
                    or checking the tool's requirements."
            ))) as Box<dyn Error + Send + Sync>)
        }
        Err(join_err) => {
            if join_err.is_panic() {
                match join_err.try_into_panic() {
                    Ok(panic_payload) => {
                        let panic_msg = extract_panic_message(panic_payload);
                        Err(Box::new(std::io::Error::other(format!(
                            "Tool '{tool_name}' panicked during execution: {panic_msg}. This may indicate an issue with \
                                the tool implementation or invalid input. Consider adjusting the tool \
                                arguments or checking the tool's requirements."
                        ))) as Box<dyn Error + Send + Sync>)
                    }
                    Err(_) => Err(Box::new(std::io::Error::other(format!(
                        "Tool '{tool_name}' panicked but could not retrieve panic payload"
                    ))) as Box<dyn Error + Send + Sync>),
                }
            } else {
                Err(Box::new(std::io::Error::other(format!(
                    "Tool '{tool_name}' task was aborted unexpectedly"
                ))) as Box<dyn Error + Send + Sync>)
            }
        }
    }
}

async fn run_tool_call(
    tools: Arc<Vec<Arc<dyn Tool>>>,
    truncation: TruncationService,
    tool_name: String,
    args: String,
    ctx: ToolContext,
) -> String {
    let tool = tools.iter().find(|t| t.name() == tool_name);
    if let Some(t) = tool {
        info!("Executing tool: {}", tool_name);
        let args_val: serde_json::Value = match serde_json::from_str(&args) {
            Ok(v) => v,
            Err(e) => return format!("Error: Failed to parse arguments: {e}"),
        };
        match execute_tool_with_panic_recovery(t.clone(), args_val, ctx).await {
            Ok(res) => {
                let truncation_result = truncation.truncate(&res.output);
                if truncation_result.truncated {
                    format!(
                        "{}\n\n[Output truncated - {} bytes saved to {}]",
                        truncation_result.content,
                        truncation_result.content.len(),
                        truncation_result
                            .output_path
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    )
                } else {
                    res.output
                }
            }
            Err(e) => {
                warn!("Tool '{}' failed: {}", tool_name, e);
                format!("Error: {e}")
            }
        }
    } else {
        format!("Tool not found: {tool_name}")
    }
}

#[derive(Clone)]
struct PreparedToolCall {
    tool_call_id: String,
    tool_name: String,
    args: String,
    ctx: ToolContext,
    start_metadata: serde_json::Value,
}

struct CompletedToolCall {
    tool_call_id: String,
    tool_name: String,
    output: String,
}

struct ToolExecutionHarness {
    tools: Arc<Vec<Arc<dyn Tool>>>,
    truncation: TruncationService,
    session_id: String,
    context_metadata: serde_json::Map<String, serde_json::Value>,
    permission_broker: Option<Arc<dyn PermissionBroker>>,
}

impl ToolExecutionHarness {
    fn new(agent: &Agent) -> Self {
        Self {
            tools: agent.tools.clone(),
            truncation: agent.truncation.clone(),
            session_id: agent.session_id.clone(),
            context_metadata: agent.tool_context_metadata.clone(),
            permission_broker: agent.permission_broker.clone(),
        }
    }

    fn policy_for(&self, tool_name: &str) -> ToolExecutionPolicy {
        self.tools
            .iter()
            .find(|tool| tool.name() == tool_name)
            .map(|tool| tool.execution_policy())
            .unwrap_or(ToolExecutionPolicy::Serial)
    }

    fn is_parallel_safe_task(&self, call: &ToolCall) -> bool {
        call.function.name == "task"
            && self.policy_for(&call.function.name) == ToolExecutionPolicy::ParallelSafe
    }

    fn parallel_wave_end(&self, calls: &[ToolCall], start: usize) -> usize {
        let mut end = start;
        while end < calls.len() && self.is_parallel_safe_task(&calls[end]) {
            end += 1;
        }
        end
    }

    fn prepare(&self, call: ToolCall, message_id: &mut String) -> PreparedToolCall {
        self.prepare_with_metadata(call, message_id, serde_json::Map::new())
    }

    fn prepare_with_metadata(
        &self,
        call: ToolCall,
        message_id: &mut String,
        extra_metadata: serde_json::Map<String, serde_json::Value>,
    ) -> PreparedToolCall {
        let tool_name = call.function.name;
        let args = call.function.arguments;
        let tool_call_id = call.id;
        let mut ctx = ToolContext::new(&self.session_id, message_id.as_str())
            .with_tool_call_id(tool_call_id.clone());
        ctx.permission_broker = self.permission_broker.clone();
        ctx.metadata.extend(self.context_metadata.clone());
        ctx.metadata.extend(extra_metadata);
        let start_metadata = self
            .tools
            .iter()
            .find(|tool| tool.name() == tool_name)
            .and_then(|tool| tool.start_metadata(&args, &ctx).ok())
            .unwrap_or(serde_json::Value::Null);
        if !start_metadata.is_null() {
            ctx.metadata
                .insert("tool_start_metadata".to_string(), start_metadata.clone());
        }
        *message_id = Uuid::new_v4().to_string();
        PreparedToolCall {
            tool_call_id,
            tool_name,
            args,
            ctx,
            start_metadata,
        }
    }

    async fn execute_one(
        &self,
        mut prepared: PreparedToolCall,
    ) -> (CompletedToolCall, Option<AgentEvent>) {
        let output = run_tool_call(
            self.tools.clone(),
            self.truncation.clone(),
            prepared.tool_name.clone(),
            prepared.args.clone(),
            prepared.ctx.clone(),
        )
        .await;

        if let Some(request) = permission_request_from_output(&output)
            && let Some(broker) = &self.permission_broker
        {
            let decision = broker.request_permission(request.clone()).await;
            let approved = decision.decision == PermissionDecisionKind::Approve;
            let event = AgentEvent::ToolPermissionResolved {
                request_id: request.id.clone(),
                approved,
            };
            if let Some(grant) = permission_grant_from_decision(&request, &decision) {
                add_permission_grant_to_context(&mut prepared.ctx, grant);
                let retry_output = run_tool_call(
                    self.tools.clone(),
                    self.truncation.clone(),
                    prepared.tool_name.clone(),
                    prepared.args.clone(),
                    prepared.ctx,
                )
                .await;
                return (
                    CompletedToolCall {
                        tool_call_id: prepared.tool_call_id,
                        tool_name: prepared.tool_name,
                        output: retry_output,
                    },
                    Some(event),
                );
            }
            return (
                CompletedToolCall {
                    tool_call_id: prepared.tool_call_id,
                    tool_name: prepared.tool_name,
                    output,
                },
                Some(event),
            );
        }

        (
            CompletedToolCall {
                tool_call_id: prepared.tool_call_id,
                tool_name: prepared.tool_name,
                output,
            },
            None,
        )
    }

    async fn execute_parallel(
        &self,
        prepared_calls: Vec<PreparedToolCall>,
    ) -> Vec<CompletedToolCall> {
        let mut handles = Vec::with_capacity(prepared_calls.len());
        for prepared in prepared_calls {
            let tools = self.tools.clone();
            let truncation = self.truncation.clone();
            handles.push(tokio::spawn(async move {
                let output = run_tool_call(
                    tools,
                    truncation,
                    prepared.tool_name.clone(),
                    prepared.args,
                    prepared.ctx,
                )
                .await;
                CompletedToolCall {
                    tool_call_id: prepared.tool_call_id,
                    tool_name: prepared.tool_name,
                    output,
                }
            }));
        }

        let mut outputs = Vec::with_capacity(handles.len());
        for handle in handles {
            outputs.push(match handle.await {
                Ok(output) => output,
                Err(err) => CompletedToolCall {
                    tool_call_id: "unknown".to_string(),
                    tool_name: "task".to_string(),
                    output: format!("Error: Tool task join failed: {err}"),
                },
            });
        }
        outputs
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
            session_id: None,
            tool_context_metadata: Default::default(),
            permission_broker: None,
        }
    }
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    pub fn set_permission_broker(&mut self, broker: Arc<dyn PermissionBroker>) {
        self.permission_broker = Some(broker);
    }

    pub async fn reset(&mut self) -> Result<()> {
        self.memory
            .clear()
            .await
            .map_err(|e| anyhow::anyhow!("Memory::clear failed: {e}"))?;
        if !self.system_prompt.is_empty() {
            self.memory
                .add_message(Message::System {
                    content: self.system_prompt.clone(),
                })
                .await
                .map_err(|e| anyhow::anyhow!("Memory::add_message failed: {e}"))?;
        }
        Ok(())
    }

    pub fn ask<'a>(
        &'a mut self,
        user_message: &'a str,
    ) -> impl Stream<Item = Result<AgentEvent>> + 'a {
        async_stream::try_stream! {
            if self.memory.messages().is_empty() && !self.system_prompt.is_empty() {
                 self.memory.add_message(Message::System { content: self.system_prompt.clone() }).await
                    .map_err(|e| anyhow::anyhow!("Memory::add_message failed: {e}"))?;
            }

            self.memory.add_message(Message::User { content: vec![ContentPart::Text { text: user_message.to_string() }] }).await
                .map_err(|e| anyhow::anyhow!("Memory::add_message failed: {e}"))?;

            let mut turn = 0;
            let mut recent_tool_calls: VecDeque<(String, String)> = VecDeque::new();
            let mut message_id = Uuid::new_v4().to_string();

            loop {
                if self.compact_if_needed().await? {
                    info!("History compacted.");
                }

                if let Some(max) = self.max_turns
                    && turn >= max
                {
                    yield AgentEvent::Error { error: "Max turns reached without final answer.".to_string() };
                    break;
                }
                turn += 1;

                compaction::prune(self.memory.messages_mut());

                let mut current_content = String::new();
                let mut current_tool_calls: Vec<ToolCall> = Vec::new();

                let provider = self.llm.clone();
                let messages = self.memory.messages().to_vec();
                debug!("LLM request messages: {:?}", messages);

                let tools_clone = self.tools.clone();
                let mut stream = provider.stream_response(&messages, tools_clone.as_slice()).await
                    .map_err(|e| anyhow::anyhow!(e))?;

                while let Some(event_result) = stream.next().await {
                    let event = event_result.map_err(|e| anyhow::anyhow!(e))?;

                    match event {
                        ProviderEvent::ContentDelta { delta } => {
                            current_content.push_str(&delta);
                            yield AgentEvent::ContentDelta { delta };
                        }
                        ProviderEvent::ReasoningDelta { delta } => {
                            yield AgentEvent::ReasoningDelta { delta };
                        }
                        ProviderEvent::ToolCallStart { id, name } => {
                            current_tool_calls.push(ToolCall {
                                id,
                                r#type: "function".to_string(),
                                function: FunctionCall {
                                    name,
                                    arguments: String::new(),
                                },
                            });
                        }
                        ProviderEvent::ToolCallDelta { id, delta } => {
                            if let Some(tc) = current_tool_calls.iter_mut().find(|tc| tc.id == id) {
                                tc.function.arguments.push_str(&delta);
                            }
                        }
                        ProviderEvent::Finish { reason } => {
                            if is_max_output_stop(reason.as_deref()) {
                                yield AgentEvent::Error {
                                    error: format!(
                                        "Model output was truncated by the provider output-token limit (finish reason: {}). Increase max_output_tokens / GEWU_MAX_OUTPUT_TOKENS and retry.",
                                        reason.as_deref().unwrap_or("unknown")
                                    ),
                                };
                                return;
                            }
                        }
                        ProviderEvent::Error { error } => {
                            yield AgentEvent::Error { error };
                            return;
                        }
                    }
                }

                let normalized_content = normalize_assistant_output(&current_content);

                let assistant_msg = Message::Assistant {
                    content: if normalized_content.is_empty() { None } else { Some(normalized_content.clone()) },
                    tool_calls: if current_tool_calls.is_empty() { None } else { Some(current_tool_calls.clone()) },
                };
                self.memory.add_message(assistant_msg).await
                    .map_err(|e| anyhow::anyhow!("Memory::add_message failed: {e}"))?;

                if current_tool_calls.is_empty() {
                    yield AgentEvent::Finish { response: normalized_content };
                    break;
                }

                info!("Agent requested {} tool calls", current_tool_calls.len());

                let turn_tool_budget = self.context_limit / 3;
                let mut turn_tool_tokens: usize = 0;
                let mut pending_consolidation_ids: Vec<String> = Vec::new();
                let mut pending_consolidation_names: Vec<String> = Vec::new();
                let mut pending_consolidation_contents: Vec<String> = Vec::new();
                let harness = ToolExecutionHarness::new(self);

                let mut call_index = 0usize;
                while call_index < current_tool_calls.len() {
                    let call = current_tool_calls[call_index].clone();
                    let tool_name = call.function.name.clone();
                    let args = call.function.arguments.clone();

                    let current_call_sig = (tool_name.clone(), args.clone());
                    recent_tool_calls.push_back(current_call_sig.clone());
                    if recent_tool_calls.len() > 3 {
                        recent_tool_calls.pop_front();
                    }

                    if recent_tool_calls.len() == 3 && recent_tool_calls.iter().all(|x| x == &current_call_sig) {
                         let error_msg = format!("Doom loop detected: Tool '{tool_name}' called 3 times repeatedly with identical arguments. Stopping.");
                         yield AgentEvent::Error { error: error_msg };
                         return;
                    }

                    if tool_name.is_empty() {
                          let error_msg = "Tool name is empty, skipping execution".to_string();
                          yield AgentEvent::Error { error: error_msg };
                          call_index += 1;
                          continue;
                     }

                    if harness.is_parallel_safe_task(&call) {
                        let block_start = call_index;
                        let block_end = harness.parallel_wave_end(&current_tool_calls, block_start);
                        let wave_parent_message_id = message_id.clone();
                        let wave_expected_count = (block_end - block_start) as i64;
                        let mut prepared_calls = Vec::new();
                        for parallel_call in current_tool_calls[block_start..block_end].iter().cloned() {
                            let mut extra_metadata = serde_json::Map::new();
                            extra_metadata.insert(
                                "parallel_wave_parent_message_id".to_string(),
                                serde_json::Value::String(wave_parent_message_id.clone()),
                            );
                            extra_metadata.insert(
                                "parallel_wave_expected_count".to_string(),
                                serde_json::Value::Number(wave_expected_count.into()),
                            );
                            let prepared = harness.prepare_with_metadata(
                                parallel_call,
                                &mut message_id,
                                extra_metadata,
                            );
                            yield AgentEvent::ToolStart {
                                tool_call_id: prepared.tool_call_id.clone(),
                                tool: prepared.tool_name.clone(),
                                input: prepared.args.clone(),
                                metadata: prepared.start_metadata.clone(),
                            };
                            prepared_calls.push(prepared);
                        }

                        for completed in harness.execute_parallel(prepared_calls).await {
                            yield AgentEvent::ToolFinish {
                                tool_call_id: completed.tool_call_id.clone(),
                                tool: completed.tool_name.clone(),
                                output: completed.output.clone(),
                            };

                            let result_tokens = compaction::estimate_tokens(&completed.output);
                            turn_tool_tokens += result_tokens;
                            pending_consolidation_ids.push(completed.tool_call_id.clone());
                            pending_consolidation_names.push(completed.tool_name.clone());
                            pending_consolidation_contents.push(completed.output.clone());
                            let media_parts = extract_media_parts(&completed.output);
                            self.memory.add_message(Message::Tool {
                                tool_call_id: completed.tool_call_id,
                                tool_name: Some(completed.tool_name),
                                content: completed.output,
                                media: media_parts,
                            }).await
                                .map_err(|e| anyhow::anyhow!("Memory::add_message failed: {e}"))?;
                        }

                        call_index = block_end;
                        continue;
                    }

                    let prepared = harness.prepare(call, &mut message_id);
                    yield AgentEvent::ToolStart {
                        tool_call_id: prepared.tool_call_id.clone(),
                        tool: prepared.tool_name.clone(),
                        input: prepared.args.clone(),
                        metadata: prepared.start_metadata.clone(),
                    };

                    let (completed, permission_event) = harness.execute_one(prepared).await;

                    if let Some(event) = permission_event {
                        yield event;
                    }

                    yield AgentEvent::ToolFinish {
                        tool_call_id: completed.tool_call_id.clone(),
                        tool: completed.tool_name.clone(),
                        output: completed.output.clone(),
                    };

                    let result_tokens = compaction::estimate_tokens(&completed.output);
                    turn_tool_tokens += result_tokens;

                    pending_consolidation_ids.push(completed.tool_call_id.clone());
                    pending_consolidation_names.push(completed.tool_name.clone());
                    pending_consolidation_contents.push(completed.output.clone());

                    let media_parts = extract_media_parts(&completed.output);
                    self.memory.add_message(Message::Tool {
                        tool_call_id: completed.tool_call_id,
                        tool_name: Some(completed.tool_name.clone()),
                        content: completed.output,
                        media: media_parts,
                    }).await
                        .map_err(|e| anyhow::anyhow!("Memory::add_message failed: {e}"))?;

                    if turn_tool_tokens > turn_tool_budget && pending_consolidation_ids.len() >= 3 {
                        info!(
                            "Tool output consolidation triggered ({} tokens > {} budget, {} calls)",
                            turn_tool_tokens, turn_tool_budget, pending_consolidation_ids.len()
                        );

                        let input = ConsolidationInput {
                            tool_call_ids: pending_consolidation_ids.clone(),
                            tool_names: pending_consolidation_names.clone(),
                            tool_contents: pending_consolidation_contents.clone(),
                        };
                        let raw_text = build_consolidation_text(&input);
                        let file_path = save_consolidation_file(&raw_text);
                        let prompt_messages = build_consolidation_prompt(&raw_text, input.tool_call_ids.len());

                        let provider = self.llm.clone();
                        match provider.stream_response(&prompt_messages, &[]).await {
                            Ok(mut summary_stream) => {
                                let mut summary_text = String::new();
                                while let Some(event_result) = summary_stream.next().await {
                                    if let Ok(ProviderEvent::ContentDelta { delta }) = event_result {
                                        summary_text.push_str(&delta);
                                    }
                                }

                                if !summary_text.is_empty() {
                                    let mut all_messages = self.memory.messages().to_vec();
                                    apply_consolidation_to_memory(
                                        &mut all_messages,
                                        &pending_consolidation_ids,
                                        &summary_text,
                                        &file_path,
                                    );
                                    self.memory.replace(all_messages).await
                                        .map_err(|e| anyhow::anyhow!("Memory::replace failed: {e}"))?;
                                    info!("Consolidated {} tool outputs into summary ({} chars)", pending_consolidation_ids.len(), summary_text.len());
                                }
                            }
                            Err(e) => {
                                warn!("Consolidation summarization failed: {}, keeping original outputs", e);
                            }
                        }

                        pending_consolidation_ids.clear();
                        pending_consolidation_names.clear();
                        pending_consolidation_contents.clear();
                        turn_tool_tokens = 0;
                    }
                    call_index += 1;
                }
            }
        }
    }

    async fn compact_if_needed(&mut self) -> Result<bool> {
        let mut total_tokens: usize = 0;
        for msg in self.memory.messages() {
            total_tokens += estimate_message_tokens(msg);
        }

        if !is_overflow(total_tokens, self.context_limit) {
            return Ok(false);
        }

        info!("Compacting history (total tokens: {})", total_tokens);

        let prompt = "Provide a detailed prompt for continuing our conversation above.
Focus on information that would be helpful for continuing the conversation, including what we did, what we're doing, which files we're working on, and what we're going to do next.
The summary that you construct will be used so that another agent can read it and continue the work.

When constructing the summary, try to stick to this template:
---
## Goal

[What goal(s) is the user trying to accomplish?]

## Instructions

- [What important instructions did the user give you that are relevant]
- [If there is a plan or spec, include information about it so next agent can continue using it]

## Discoveries

[What notable things were learned during this conversation that would be useful for the next agent to know when continuing the work]

## Accomplished

[What work has been completed, what work is still in progress, and what work is left?]

## Relevant files / directories

[Construct a structured list of relevant files that have been read, edited, or created that pertain to the task at hand. If all the files in a directory are relevant, include the path to the directory.]
---";

        let mut request_messages = self.memory.messages().to_vec();
        request_messages.push(Message::User {
            content: vec![ContentPart::Text {
                text: prompt.to_string(),
            }],
        });

        let provider = self.llm.clone();
        let mut stream = provider
            .stream_response(&request_messages, &[])
            .await
            .map_err(|e| anyhow::anyhow!("Compaction failed: {e}"))?;

        let mut summary_text = String::new();
        while let Some(event_result) = stream.next().await {
            let event =
                event_result.map_err(|e| anyhow::anyhow!("Compaction stream error: {e}"))?;
            if let ProviderEvent::ContentDelta { delta } = event {
                summary_text.push_str(&delta);
            }
        }

        if summary_text.is_empty() {
            compaction::prune(self.memory.messages_mut());
            if is_overflow(
                self.memory
                    .messages()
                    .iter()
                    .map(estimate_message_tokens)
                    .sum(),
                self.context_limit,
            ) {
                return Err(anyhow::anyhow!(
                    "Compaction failed: LLM returned empty summary and pruning was insufficient"
                ));
            }
            return Ok(false);
        }

        let mut new_history = Vec::new();

        if let Some(sys_msg) = self
            .memory
            .messages()
            .iter()
            .find(|m| matches!(m, Message::System { .. }))
        {
            new_history.push(sys_msg.clone());
        }

        new_history.push(Message::Assistant {
            content: Some(format!("Summary of previous conversation:\n{summary_text}")),
            tool_calls: None,
        });

        if let Some(last_user_msg) = self
            .memory
            .messages()
            .iter()
            .rev()
            .find(|m| matches!(m, Message::User { .. }))
        {
            new_history.push(last_user_msg.clone());
        }

        self.memory
            .replace(new_history)
            .await
            .map_err(|e| anyhow::anyhow!("Memory::replace failed: {e}"))?;
        Ok(true)
    }
}

#[cfg(test)]
mod normalization_tests {
    use super::normalize_assistant_output;

    #[test]
    fn strips_think_analysis_and_reasoning_blocks() {
        let raw = "<think>internal</think> {\"ok\":true} <analysis>drop</analysis> <reasoning>drop2</reasoning>";
        let cleaned = normalize_assistant_output(raw);
        assert_eq!(cleaned, "{\"ok\":true}");
    }

    #[test]
    fn preserves_regular_text_when_no_tags_present() {
        let raw = "plain response";
        let cleaned = normalize_assistant_output(raw);
        assert_eq!(cleaned, "plain response");
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
            None => Err(format!("Tool not found: {name}").into()),
        }
    }

    fn list_tools(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    fn execution_policy(&self, name: &str) -> Option<ToolExecutionPolicy> {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(|tool| tool.execution_policy())
    }
}

impl AgentBuilder {
    pub fn llm(mut self, llm: Arc<dyn LlmProvider>) -> Self {
        self.llm = Some(llm);
        self
    }

    pub fn with_tools(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.tools = Arc::new(tools);
        self
    }

    pub fn system_prompt(mut self, prompt: &str) -> Self {
        self.system_prompt = prompt.to_string();
        self
    }

    pub fn max_turns(mut self, max_turns: Option<usize>) -> Self {
        self.max_turns = max_turns;
        self
    }

    pub fn context_limit(mut self, limit: usize) -> Self {
        self.context_limit = limit;
        self
    }

    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn tool_context_metadata(
        mut self,
        metadata: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        self.tool_context_metadata = metadata;
        self
    }

    pub fn permission_broker(mut self, broker: Arc<dyn PermissionBroker>) -> Self {
        self.permission_broker = Some(broker);
        self
    }

    pub fn memory(mut self, memory: Box<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    pub fn build(self) -> Result<Agent> {
        let llm = self.llm.context("LLM provider is required")?;
        let agent = Agent {
            llm,
            tools: self.tools,
            system_prompt: self.system_prompt.clone(),
            max_turns: self.max_turns,
            memory: self.memory.unwrap_or_else(|| Box::new(Vec::new())),
            context_limit: self.context_limit,
            truncation: TruncationService::new(),
            session_id: self
                .session_id
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            tool_context_metadata: self.tool_context_metadata,
            permission_broker: self.permission_broker,
        };
        Ok(agent)
    }
}

#[derive(Deserialize)]
struct MediaPayload {
    #[serde(rename = "type")]
    kind: String,
    mime: String,
    data: String,
    #[serde(default)]
    message: Option<String>,
}

fn extract_media_parts(content: &str) -> Vec<ContentPart> {
    let parsed: MediaPayload = match serde_json::from_str(content) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };

    if parsed.kind != "media" {
        return Vec::new();
    }

    let mut parts = Vec::new();
    if let Some(text) = parsed.message {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            parts.push(ContentPart::Text {
                text: trimmed.to_string(),
            });
        }
    }

    let mime = parsed.mime.as_str();
    let content_part = if mime.starts_with("image/") {
        ContentPart::Image {
            data: parsed.data,
            mime_type: parsed.mime,
        }
    } else if mime.starts_with("audio/") {
        ContentPart::Audio {
            data: parsed.data,
            mime_type: parsed.mime,
        }
    } else if mime.starts_with("video/") {
        ContentPart::Video {
            data: parsed.data,
            mime_type: parsed.mime,
        }
    } else {
        ContentPart::File {
            data: parsed.data,
            mime_type: parsed.mime,
        }
    };
    parts.push(content_part);

    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_media_parts_parses_image() {
        let json = r#"{"type":"media","mime":"image/png","data":"ZmFrZQ==","message":"Image read successfully"}"#;
        let parts = extract_media_parts(json);
        assert_eq!(parts.len(), 2);
        match &parts[0] {
            ContentPart::Text { text } => assert!(text.contains("Image")),
            _ => panic!("first part should be text"),
        }
        match &parts[1] {
            ContentPart::Image { mime_type, data } => {
                assert_eq!(mime_type, "image/png");
                assert_eq!(data, "ZmFrZQ==");
            }
            _ => panic!("second part should be image"),
        }
    }

    #[test]
    fn extract_media_parts_ignores_non_media() {
        let json = r#"{"type":"text","text":"hello"}"#;
        let parts = extract_media_parts(json);
        assert!(parts.is_empty());
    }
}
