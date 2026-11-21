//! Execution utilities for the agent loop.
//!
//! This module provides helper structs and functions used by the [`Agent::ask`](super::Agent::ask)
//! method to modularize the main agent loop logic.

use crate::types::{
    AgentEvent, ContentPart, FunctionCall, Message, ProviderEvent, ToolCall, ToolContext,
    ToolResult,
};
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
    pub(super) fn check(&self, tool: &str, args: &str) -> Option<String> {
        // The current call counts toward the streak: execution is stopped ON
        // the threshold-th consecutive identical call, not one call after it
        // (`record` only runs for calls that pass this check, so without
        // counting the current call the detector would need threshold + 1
        // calls to fire — contradicting the documented "3 identical calls
        // stops execution").
        if repeats_at_threshold(&self.recent_calls, tool, args, self.threshold) {
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

fn repeats_at_threshold(
    recent_calls: &VecDeque<(String, String)>,
    tool: &str,
    args: &str,
    threshold: usize,
) -> bool {
    let needed = threshold.saturating_sub(1);
    recent_calls.len() >= needed
        && recent_calls
            .iter()
            .rev()
            .take(needed)
            .all(|(name, arguments)| name == tool && arguments == args)
}

pub(super) struct AggregatedResponse {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub replay: Option<crate::types::ReasoningReplay>,
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
    pub(super) fn take_parts(&mut self) -> AggregatedResponse {
        AggregatedResponse {
            content: std::mem::take(&mut self.content),
            reasoning: std::mem::take(&mut self.reasoning),
            tool_calls: std::mem::take(&mut self.tool_calls),
            replay: self.replay.take(),
        }
    }
}

pub(super) enum ToolCallPlan {
    Stop(String),
    Reject(ToolResult),
    Execute,
}

pub(super) fn plan_tool_call(call: &ToolCall, doom_loop: &mut DoomLoopDetector) -> ToolCallPlan {
    let name = &call.function.name;
    let args = &call.function.arguments;
    if let Some(error) = doom_loop.check(name, args) {
        return ToolCallPlan::Stop(error);
    }
    doom_loop.record(name.clone(), args.clone());
    if name.is_empty() {
        ToolCallPlan::Reject(ToolResult::new(
            name.clone(),
            "Error: the tool call had an empty name and was not executed.",
        ))
    } else {
        ToolCallPlan::Execute
    }
}

pub(super) struct NormalizedToolResult {
    pub error: Option<AgentEvent>,
    pub finish: AgentEvent,
    pub message: Message,
}

pub(super) fn normalize_tool_result(
    call: &ToolCall,
    result: Option<Result<ToolResult>>,
) -> NormalizedToolResult {
    let tool_name = call.function.name.clone();
    let (result, error) = match result {
        Some(Ok(result)) => (result, None),
        Some(Err(e)) => (
            ToolResult::new(tool_name.clone(), format!("Error: {}", e)),
            Some(AgentEvent::Error {
                error: e.to_string(),
            }),
        ),
        None => (
            ToolResult::new(
                tool_name.clone(),
                "Error: the tool call did not produce a result (execution task failed).",
            ),
            None,
        ),
    };
    // Malformed arguments are answered with a normal tool result so history
    // stays protocol-valid, but the parse failure is still surfaced as an
    // error event for the UI.
    let error = error.or_else(|| {
        result
            .output
            .starts_with("Error: invalid arguments")
            .then(|| AgentEvent::Error {
                error: result.output.clone(),
            })
    });
    let finish = AgentEvent::ToolFinish {
        call_id: call.id.clone(),
        tool: tool_name.clone(),
        title: result.title,
        output: result.output.clone(),
        metadata: result.metadata,
    };
    let media = extract_media_parts(&result.output);
    let content = if media.is_empty() {
        result.output
    } else {
        match media.iter().find(|p| matches!(p, ContentPart::Text { .. })) {
            Some(ContentPart::Text { text }) => text.clone(),
            _ => "[media attached]".to_string(),
        }
    };
    NormalizedToolResult {
        error,
        finish,
        message: Message::Tool {
            tool_call_id: call.id.clone(),
            tool_name: Some(tool_name),
            content,
            media,
        },
    }
}

pub(super) fn prepare_provider_messages(history: &[Message], system_prompt: &str) -> Vec<Message> {
    let mut messages = crate::compaction::active_context(history).to_messages();
    if !system_prompt.is_empty() {
        if let Some(Message::System { content }) = messages.first_mut() {
            *content = system_prompt.to_string();
        } else {
            messages.insert(
                0,
                Message::System {
                    content: system_prompt.to_string(),
                },
            );
        }
    }
    messages
}

#[derive(Debug, PartialEq)]
pub(super) enum TurnDisposition {
    Tools,
    Steered,
    Finish,
}

pub(super) fn classify_turn(tool_calls: &[ToolCall], steering_pending: bool) -> TurnDisposition {
    if !tool_calls.is_empty() {
        TurnDisposition::Tools
    } else if steering_pending {
        TurnDisposition::Steered
    } else {
        TurnDisposition::Finish
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
#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "id".into(),
            r#type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }

    #[test]
    fn aggregator_returns_named_parts_and_resets() {
        let mut aggregator = LlmResponseAggregator::new();
        aggregator.process_event(ProviderEvent::ContentDelta { delta: "é".into() });
        aggregator.process_event(ProviderEvent::ReasoningDelta {
            delta: "think".into(),
        });
        aggregator.process_event(ProviderEvent::ToolCallStart {
            id: "id".into(),
            name: "tool".into(),
        });
        aggregator.process_event(ProviderEvent::ToolCallDelta {
            id: "id".into(),
            delta: "{}".into(),
        });
        let response = aggregator.take_parts();
        assert_eq!(response.content, "é");
        assert_eq!(response.reasoning, "think");
        assert_eq!(response.tool_calls[0].function.arguments, "{}");
        assert!(aggregator.take_parts().content.is_empty());
    }

    #[test]
    fn turn_classification_prioritizes_tool_calls() {
        assert_eq!(classify_turn(&[], false), TurnDisposition::Finish);
        assert_eq!(classify_turn(&[], true), TurnDisposition::Steered);
        assert_eq!(
            classify_turn(&[call("tool", "{}")], true),
            TurnDisposition::Tools
        );
    }

    #[test]
    fn provider_message_preparation_replaces_or_inserts_system_prompt() {
        let history = vec![Message::User {
            content: vec![ContentPart::Text { text: "hi".into() }],
        }];
        let messages = prepare_provider_messages(&history, "system");
        assert!(matches!(&messages[0], Message::System { content } if content == "system"));
        let history = vec![Message::System {
            content: "old".into(),
        }];
        let messages = prepare_provider_messages(&history, "new");
        assert!(matches!(&messages[0], Message::System { content } if content == "new"));
    }

    #[test]
    fn normalization_keeps_raw_media_in_event_but_not_history_text() {
        for (message, expected) in [
            (Some(" reference "), "reference"),
            (None, "[media attached]"),
        ] {
            let output = serde_json::json!({
                "type": "media", "mime": "image/png", "data": "ZmFrZQ==", "message": message
            })
            .to_string();
            let mut result = ToolResult::new("image title", output.clone());
            result.metadata.insert("key".into(), serde_json::json!(1));
            let normalized = normalize_tool_result(&call("tool", "{}"), Some(Ok(result)));
            assert!(normalized.error.is_none());
            assert!(
                matches!(normalized.finish, AgentEvent::ToolFinish { tool, title, output: raw, metadata, .. }
                if tool == "tool" && title == "image title" && raw == output && metadata["key"] == 1)
            );
            assert!(
                matches!(normalized.message, Message::Tool { tool_call_id, tool_name, content, media }
                if tool_call_id == "id" && tool_name.as_deref() == Some("tool") && content == expected
                && matches!(media.last(), Some(ContentPart::Image { data, .. }) if data == "ZmFrZQ=="))
            );
        }
    }

    #[test]
    fn normalization_answers_lost_tasks_without_an_error_event() {
        let normalized = normalize_tool_result(&call("tool", "{}"), None);
        assert!(normalized.error.is_none());
        assert!(
            matches!(normalized.message, Message::Tool { content, media, .. }
            if content == "Error: the tool call did not produce a result (execution task failed)." && media.is_empty())
        );
    }

    #[test]
    fn normalization_preserves_error_and_tool_message() {
        let normalized =
            normalize_tool_result(&call("tool", "{}"), Some(Err(anyhow::anyhow!("failed"))));
        assert!(matches!(normalized.error, Some(AgentEvent::Error { .. })));
        assert!(matches!(normalized.finish, AgentEvent::ToolFinish { .. }));
        assert!(matches!(normalized.message, Message::Tool { .. }));
    }

    #[test]
    fn repetition_predicate_counts_current_call_and_only_matching_suffix() {
        let recent = VecDeque::from([
            ("other".into(), "{}".into()),
            ("tool".into(), "{}".into()),
            ("tool".into(), "{}".into()),
        ]);
        assert!(repeats_at_threshold(&recent, "tool", "{}", 3));
        assert!(!repeats_at_threshold(&recent, "tool", "{}", 4));
        assert!(!repeats_at_threshold(&recent, "other", "{}", 2));
        assert!(!repeats_at_threshold(&recent, "tool", "{ }", 2));
        assert!(!repeats_at_threshold(&VecDeque::new(), "tool", "{}", 2));
        for threshold in [0, 1] {
            assert!(repeats_at_threshold(
                &VecDeque::new(),
                "tool",
                "{}",
                threshold
            ));
        }
    }

    #[test]
    fn tool_call_planning_records_empty_names_and_detects_streaks() {
        let mut detector = DoomLoopDetector::new(2);
        assert!(matches!(
            plan_tool_call(&call("", "{}"), &mut detector),
            ToolCallPlan::Reject(_)
        ));
        assert!(matches!(
            plan_tool_call(&call("tool", "{}"), &mut detector),
            ToolCallPlan::Execute
        ));
        assert!(matches!(
            plan_tool_call(&call("tool", "{}"), &mut detector),
            ToolCallPlan::Stop(_)
        ));
    }
}
