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
use crate::middleware::TruncationService;
use crate::traits::{LlmProvider, Memory, Tool, ToolRegistry};
use crate::types::{
    AgentEvent, ContentPart, FunctionCall, Message, ProviderEvent, ToolCall, ToolContext,
    ToolResult,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde::Deserialize;
use std::any::Any;
use std::collections::VecDeque;
use std::error::Error;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use tokio::task::spawn_blocking;
use tracing::{debug, info, warn};
use uuid::Uuid;

fn strip_tag_blocks(input: &str, tag: &str) -> String {
    let start_tag = format!("<{}>", tag);
    let end_tag = format!("</{}>", tag);
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

pub struct Agent {
    llm: Arc<dyn LlmProvider>,
    tools: Arc<Vec<Arc<dyn Tool>>>,
    system_prompt: String,
    max_turns: Option<usize>,
    pub memory: Box<dyn Memory>,
    pub context_limit: usize,
    truncation: TruncationService,
    session_id: String,
}

pub struct AgentBuilder {
    llm: Option<Arc<dyn LlmProvider>>,
    tools: Arc<Vec<Arc<dyn Tool>>>,
    system_prompt: String,
    max_turns: Option<usize>,
    memory: Option<Box<dyn Memory>>,
    context_limit: usize,
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
    }).await;

    match result {
        Ok(Ok(res)) => res,
        Ok(Err(e)) => {
            let panic_msg = extract_panic_message(e);
            Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "Tool '{}' panicked during execution: {}. This may indicate an issue with \
                    the tool implementation or invalid input. Consider adjusting the tool arguments \
                    or checking the tool's requirements.",
                    tool_name, panic_msg
                ),
            )) as Box<dyn Error + Send + Sync>)
        }
        Err(join_err) => {
            if join_err.is_panic() {
                match join_err.try_into_panic() {
                    Ok(panic_payload) => {
                        let panic_msg = extract_panic_message(panic_payload);
                        Err(Box::new(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!(
                                "Tool '{}' panicked during execution: {}. This may indicate an issue with \
                                the tool implementation or invalid input. Consider adjusting the tool \
                                arguments or checking the tool's requirements.",
                                tool_name, panic_msg
                            ),
                        )) as Box<dyn Error + Send + Sync>)
                    }
                    Err(_) => {
                        Err(Box::new(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("Tool '{}' panicked but could not retrieve panic payload", tool_name),
                        )) as Box<dyn Error + Send + Sync>)
                    }
                }
            } else {
                Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Tool '{}' task was aborted unexpectedly", tool_name),
                )) as Box<dyn Error + Send + Sync>)
            }
        }
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
        }
    }
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

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

    pub fn ask<'a>(
        &'a mut self,
        user_message: &'a str,
    ) -> impl Stream<Item = Result<AgentEvent>> + 'a {
        async_stream::try_stream! {
            if self.memory.messages().is_empty() && !self.system_prompt.is_empty() {
                 self.memory.add_message(Message::System { content: self.system_prompt.clone() }).await;
            }

            self.memory.add_message(Message::User { content: vec![ContentPart::Text { text: user_message.to_string() }] }).await;

            let mut turn = 0;
            let mut recent_tool_calls: VecDeque<(String, String)> = VecDeque::new();
            let mut message_id = Uuid::new_v4().to_string();

            loop {
                if self.compact_if_needed().await? {
                    info!("History compacted.");
                }

                if let Some(max) = self.max_turns {
                    if turn >= max {
                        yield AgentEvent::Error { error: "Max turns reached without final answer.".to_string() };
                        break;
                    }
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
                        ProviderEvent::Finish { .. } => {
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
                self.memory.add_message(assistant_msg).await;

                if current_tool_calls.is_empty() {
                    yield AgentEvent::Finish { response: normalized_content };
                    break;
                }

                info!("Agent requested {} tool calls", current_tool_calls.len());

                for call in current_tool_calls {
                    let tool_name = call.function.name.clone();
                    let args = call.function.arguments.clone();
                    let call_id = call.id;

                    let current_call_sig = (tool_name.clone(), args.clone());
                    recent_tool_calls.push_back(current_call_sig.clone());
                    if recent_tool_calls.len() > 3 {
                        recent_tool_calls.pop_front();
                    }

                    if recent_tool_calls.len() == 3 && recent_tool_calls.iter().all(|x| x == &current_call_sig) {
                         let error_msg = format!("Doom loop detected: Tool '{}' called 3 times repeatedly with identical arguments. Stopping.", tool_name);
                         yield AgentEvent::Error { error: error_msg };
                         return;
                    }

                    if tool_name.is_empty() {
                         let error_msg = "Tool name is empty, skipping execution".to_string();
                         yield AgentEvent::Error { error: error_msg };
                         continue;
                    }

                    yield AgentEvent::ToolStart { tool: tool_name.clone(), input: args.clone() };

                    let ctx = ToolContext::new(&self.session_id, &message_id);
                    message_id = Uuid::new_v4().to_string();

                    let tool = self.tools.iter().find(|t| t.name() == tool_name);

                    let result_content = if let Some(t) = tool {
                        info!("Executing tool: {}", tool_name);
                        let args_val: serde_json::Value = match serde_json::from_str(&args) {
                            Ok(v) => v,
                            Err(e) => {
                                yield AgentEvent::Error { error: format!("Failed to parse arguments: {}", e) };
                                continue;
                            }
                        };
                        match execute_tool_with_panic_recovery(t.clone(), args_val, ctx.clone()).await {
                            Ok(res) => {
                                let truncation_result = self.truncation.truncate(&res.output, false);
                                let output = if truncation_result.truncated {
                                    format!(
                                        "{}\n\n[Output truncated - {} bytes saved to {}]",
                                        truncation_result.content,
                                        truncation_result.content.len(),
                                        truncation_result.output_path.map(|p| p.display().to_string()).unwrap_or_default()
                                    )
                                } else {
                                    res.output
                                };
                                output
                            }
                            Err(e) => {
                                warn!("Tool '{}' failed: {}", tool_name, e);
                                format!("Error: {}", e)
                            }
                        }
                    } else {
                        format!("Tool not found: {}", tool_name)
                    };

                    yield AgentEvent::ToolFinish { tool: tool_name.clone(), output: result_content.clone() };

                    let media_parts = extract_media_parts(&result_content);
                    self.memory.add_message(Message::Tool {
                        tool_call_id: call_id,
                        tool_name: Some(tool_name.clone()),
                        content: result_content,
                        media: media_parts,
                    }).await;
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
            .map_err(|e| anyhow::anyhow!("Compaction failed: {}", e))?;

        let mut summary_text = String::new();
        while let Some(event_result) = stream.next().await {
            let event =
                event_result.map_err(|e| anyhow::anyhow!("Compaction stream error: {}", e))?;
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
                    .map(|m| estimate_message_tokens(m))
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
            content: Some(format!(
                "Summary of previous conversation:\n{}",
                summary_text
            )),
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

        self.memory.replace(new_history).await;
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
            None => Err(format!("Tool not found: {}", name).into()),
        }
    }

    fn list_tools(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
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
            session_id: Uuid::new_v4().to_string(),
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
