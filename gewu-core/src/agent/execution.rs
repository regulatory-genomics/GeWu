//! Execution utilities for the agent loop.
//!
//! This module provides helper structs and functions used by the [`Agent::ask`](super::Agent::ask)
//! method to modularize the main agent loop logic.

use crate::types::{AgentEvent, FunctionCall, ProviderEvent, ToolCall, ToolContext, ToolResult};
use anyhow::Result;
use std::collections::VecDeque;
use tracing::warn;

use super::execute_tool_with_panic_recovery;
use super::media::extract_media_parts;
use crate::middleware::TruncationService;
use crate::traits::Tool;

/// Tracks tool calls to detect doom loops.
///
/// A doom loop occurs when the same tool is called multiple times with
/// identical arguments, which typically indicates the agent is stuck
/// in an infinite cycle. This detector maintains a sliding window
/// of recent tool calls and flags when the same call appears
/// `threshold` times consecutively.
pub(super) struct DoomLoopDetector {
    recent_calls: VecDeque<(String, String)>,
    threshold: usize,
}

impl DoomLoopDetector {
    /// Creates a new `DoomLoopDetector` with the specified threshold.
    ///
    /// # Arguments
    ///
    /// * `threshold` - Number of consecutive identical calls to trigger doom loop detection
    pub(super) fn new(threshold: usize) -> Self {
        Self {
            recent_calls: VecDeque::new(),
            threshold,
        }
    }

    /// Checks if the given tool call would trigger doom loop detection.
    ///
    /// Returns `Some(error_message)` if doom loop is detected, `None` otherwise.
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool name
    /// * `args` - The tool arguments
    pub(super) fn check(&mut self, tool: &str, args: &str) -> Option<String> {
        let current_call_sig = (tool.to_string(), args.to_string());
        // The current call counts toward the streak: execution is stopped ON
        // the threshold-th consecutive identical call, not one call after it
        // (`record` only runs for calls that pass this check, so without
        // counting the current call the detector would need threshold + 1
        // calls to fire — contradicting the documented "3 identical calls
        // stops execution").
        let needed = self.threshold.saturating_sub(1);
        if self.recent_calls.len() >= needed
            && self
                .recent_calls
                .iter()
                .rev()
                .take(needed)
                .all(|x| x == &current_call_sig)
        {
            return Some(format!(
                "Doom loop detected: Tool '{}' called {} times repeatedly with identical arguments. Stopping.",
                tool, self.threshold
            ));
        }
        None
    }

    /// Records a tool call for future doom loop detection.
    ///
    /// Adds the tool call to the recent calls history and removes
    /// the oldest call if the history exceeds the threshold.
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool name
    /// * `args` - The tool arguments
    pub(super) fn record(&mut self, tool: String, args: String) {
        self.recent_calls.push_back((tool, args));
        if self.recent_calls.len() > self.threshold {
            self.recent_calls.pop_front();
        }
    }
}

/// Aggregates streaming LLM responses into content, reasoning, and tool calls.
///
/// The `LlmResponseAggregator` processes [`ProviderEvent`] items from the LLM
/// stream, collecting content and reasoning deltas and building up complete
/// tool calls from start/delta events. It yields [`AgentEvent`] items that
/// should be immediately forwarded to the caller.
pub(super) struct LlmResponseAggregator {
    content: String,
    reasoning: String,
    replay: Option<crate::types::ReasoningReplay>,
    tool_calls: Vec<ToolCall>,
}

impl LlmResponseAggregator {
    /// Creates a new empty aggregator.
    pub(super) fn new() -> Self {
        Self {
            content: String::new(),
            reasoning: String::new(),
            replay: None,
            tool_calls: Vec::new(),
        }
    }

    /// Processes a single provider event.
    ///
    /// For content and reasoning deltas, returns an `AgentEvent` to yield.
    /// For tool call events, accumulates the data internally and returns `None`.
    /// For errors, returns an `AgentEvent` to yield.
    ///
    /// # Arguments
    ///
    /// * `event` - The provider event to process
    ///
    /// # Returns
    ///
    /// `Some(AgentEvent)` if the event should be yielded, `None` if it's internal
    pub(super) fn process_event(&mut self, event: ProviderEvent) -> Option<AgentEvent> {
        match event {
            ProviderEvent::ReasoningReplay { replay } => {
                self.replay = Some(replay);
                None
            }
            ProviderEvent::ContentDelta { delta } => {
                self.content.push_str(&delta);
                Some(AgentEvent::ContentDelta { delta })
            }
            ProviderEvent::ReasoningDelta { delta } => {
                self.reasoning.push_str(&delta);
                Some(AgentEvent::ReasoningDelta { delta })
            }
            ProviderEvent::ToolCallStart { id, name } => {
                self.tool_calls.push(ToolCall {
                    id,
                    r#type: "function".to_string(),
                    function: FunctionCall {
                        name,
                        arguments: String::new(),
                    },
                });
                None
            }
            ProviderEvent::ToolCallDelta { id, delta } => {
                if let Some(tc) = self.tool_calls.iter_mut().find(|tc| tc.id == id) {
                    tc.function.arguments.push_str(&delta);
                }
                None
            }
            ProviderEvent::Finish { .. } => None,
            // Provider usage is folded into the agent's session totals by
            // the ask loop (and compaction); the per-request figure is not
            // forwarded as an agent event.
            ProviderEvent::Usage { .. } => None,
            ProviderEvent::Error { error } => Some(AgentEvent::Error { error }),
        }
    }

    /// Extracts and returns the accumulated content, reasoning, and tool calls.
    ///
    /// After calling this method, the aggregator is empty and ready
    /// to aggregate a new response.
    ///
    /// # Returns
    ///
    /// Content, display reasoning, tool calls, and native reasoning replay state.
    pub(super) fn take_parts(
        &mut self,
    ) -> (
        String,
        String,
        Vec<ToolCall>,
        Option<crate::types::ReasoningReplay>,
    ) {
        (
            std::mem::take(&mut self.content),
            std::mem::take(&mut self.reasoning),
            std::mem::take(&mut self.tool_calls),
            self.replay.take(),
        )
    }
}

/// Executes a single tool call with panic recovery and output truncation.
///
/// This function handles the complete lifecycle of a tool execution:
/// 1. Parses the JSON arguments
/// 2. Executes the tool with panic recovery
/// 3. Truncates the output if necessary
/// 4. Returns the tool result with its output truncated if necessary
///
/// Failures are reported as error `ToolResult`s (never `Err`) so the caller
/// always records a tool message for every tool call — an assistant turn with
/// unanswered `tool_calls` is rejected by strict providers.
///
/// # Arguments
///
/// * `tool` - The tool to execute
/// * `call` - The tool call containing the function name and arguments
/// * `ctx` - The execution context
/// * `truncation` - The truncation service for handling long outputs
///
/// # Returns
///
/// The tool result; argument-parse failures come back as an error result
pub(super) async fn execute_tool_call(
    tool: std::sync::Arc<dyn Tool>,
    call: ToolCall,
    ctx: ToolContext,
    truncation: &TruncationService,
) -> Result<ToolResult> {
    let tool_name = tool.name().to_string();
    let args = call.function.arguments;

    let args_val: serde_json::Value = match serde_json::from_str(&args) {
        Ok(value) => value,
        Err(e) => {
            warn!("Tool '{}' received invalid arguments: {}", tool_name, e);
            return Ok(ToolResult::new(
                tool_name,
                format!(
                    "Error: invalid arguments ({}). The arguments must be a valid JSON object \
                     matching the tool's input schema. Re-issue the call with corrected arguments.",
                    e
                ),
            ));
        }
    };

    match execute_tool_with_panic_recovery(tool, args_val, ctx).await {
        Ok(mut res) => {
            // Media payloads (e.g. `read_file` on an image/PDF) must reach the
            // agent's media extractor intact. Truncating them mangles the
            // base64 JSON and the image is silently lost, so those outputs
            // bypass truncation entirely.
            if extract_media_parts(&res.output).is_empty() {
                let original_len = res.output.len();
                let truncation_result = truncation.truncate(&res.output);
                if truncation_result.truncated {
                    let saved_note = match &truncation_result.output_path {
                        Some(path) => format!("full output saved to {}", path.display()),
                        None => "full output could not be saved to disk".to_string(),
                    };
                    res.output = format!(
                        "{}\n\n[Output truncated - {} of {} bytes shown; {}]",
                        truncation_result.content,
                        truncation_result.content.len(),
                        original_len,
                        saved_note
                    );
                }
            }
            Ok(res)
        }
        Err(e) => {
            warn!("Tool '{}' failed: {}", tool_name, e);
            Ok(ToolResult::new(tool_name, format!("Error: {}", e)))
        }
    }
}
