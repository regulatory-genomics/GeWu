//! History compaction and token management.
//!
//! This module provides functionality for managing conversation history size:
//!
//! - [`estimate_tokens`] - Rough token estimation for text
//! - [`estimate_message_tokens`] - Token estimation for complete messages
//! - [`is_overflow`] - Determines if history exceeds the token limit
//! - [`prune`] - Removes old tool outputs to free up context space
//!
//! # Token Estimation
//!
//! Token estimation is intentionally simplified: `text.len() / 4`. This is a
//! rough approximation where each token is assumed to be ~4 characters.
//! For binary media data (base64), the same approximation is used.
//!
//! # Compaction vs Pruning
//!
//! GeWu uses two strategies to manage context size:
//!
//! - **Pruning** ([`prune`]): Fast, in-memory cleanup that removes tool output
//!   content from old messages. Keeps the structure but replaces long outputs
//!   with "[Output pruned]". Triggered on every agent turn.
//!
//! - **Compaction** ([`Agent::compact_if_needed`](super::agent::Agent::compact_if_needed)):
//!   Full summarization that uses the LLM to create a summary of the entire
//!   conversation history. Only triggered when history exceeds context limit.

use crate::types::{ContentPart, Message};

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;
pub const PRUNE_PROTECT_THRESHOLD: usize = 40_000;
pub const PRUNE_MINIMUM_SAVINGS: usize = 20_000;
pub const COMPACTION_BUFFER: usize = 20_000;
pub const CONSOLIDATION_DIR: &str = ".gewu/tool-consolidation";

/// Estimates the number of tokens in a text string.
///
/// Uses a simple approximation: `text.len() / 4`. This assumes average
/// token length of ~4 characters, which is a reasonable approximation
/// for English text. Note that actual tokenizers (like tiktoken) may
/// produce different counts.
pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

/// Determines if the current token count exceeds the context limit.
///
/// Returns `true` if compaction should be considered. A buffer of
/// [`COMPACTION_BUFFER`] tokens is reserved to ensure some working space
/// remains after compaction.
///
/// # Arguments
///
/// * `total_tokens` - The estimated total tokens in the conversation history.
/// * `limit_context` - The maximum context window size (e.g., 128,000 for GPT-4).
///
/// # Examples
///
/// ```
/// use gewu_core::compaction::is_overflow;
///
/// // With an 8k context limit and 120k tokens, overflow is true
/// assert!(is_overflow(120_000, 128_000));
///
/// // With an 8k context limit and 100k tokens, overflow is false
/// assert!(!is_overflow(100_000, 128_000));
/// ```
pub fn is_overflow(total_tokens: usize, limit_context: usize) -> bool {
    if limit_context == 0 {
        return false;
    }
    let reserved = COMPACTION_BUFFER;
    let usable = limit_context.saturating_sub(reserved);
    total_tokens >= usable
}

fn estimate_content_tokens(content: &[ContentPart]) -> usize {
    content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => estimate_tokens(text),
            ContentPart::Image { data, .. } => estimate_tokens(data),
            ContentPart::Audio { data, .. } => estimate_tokens(data),
            ContentPart::Video { data, .. } => estimate_tokens(data),
            ContentPart::File { data, .. } => estimate_tokens(data),
        })
        .sum()
}

/// Estimates the token count for a complete message.
///
/// Token estimation varies by message type:
/// - **System**: Simple text length
/// - **User**: Sum of all content parts (text + media data)
/// - **Assistant**: Text content plus function names and arguments from tool calls
/// - **Tool**: Original content plus any additional media parts
pub fn estimate_message_tokens(msg: &Message) -> usize {
    match msg {
        Message::System { content } => estimate_tokens(content),
        Message::User { content } => estimate_content_tokens(content),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let mut count = content.as_ref().map(|s| estimate_tokens(s)).unwrap_or(0);
            if let Some(calls) = tool_calls {
                for call in calls {
                    count += estimate_tokens(&call.function.arguments);
                    count += estimate_tokens(&call.function.name);
                }
            }
            count
        }
        Message::Tool { content, media, .. } => {
            let mut count = estimate_tokens(content);
            count += estimate_content_tokens(media);
            count
        }
    }
}

/// Prunes tool outputs from older messages to free context space.
///
/// This function walks backwards through the message history, skipping the
/// most recent 2 turns (to keep recent context), and marks old tool outputs
/// for pruning once the total tool output size exceeds [`PRUNE_PROTECT_THRESHOLD`].
///
/// A message starting with "Summary:" is treated as a compaction summary
/// and stops the pruning process (preserving the summarized history intact).
///
/// # Arguments
///
/// * `messages` - The message history to prune (modified in place).
///
/// # Returns
///
/// The number of tokens saved by pruning, or 0 if pruning was not performed
/// (either not enough savings or pruning threshold not reached).
///
/// # Pruning Behavior
///
/// 1. Iterates backwards from the second-to-last message
/// 2. Skips the 2 most recent turns (user/assistant pairs)
/// 3. Accumulates tool output tokens until exceeding `PRUNE_PROTECT_THRESHOLD`
/// 4. Once threshold exceeded, marks those messages for pruning
/// 5. Only actually prunes if total savings exceed `PRUNE_MINIMUM_SAVINGS`
/// 6. Replaces tool content with "[Output pruned]" and clears media
pub fn prune(messages: &mut [Message]) -> usize {
    let mut total_tool_tokens = 0;
    let mut pruned_tokens = 0;
    let mut to_prune = Vec::new();
    let mut turns = 0;

    for (i, msg) in messages.iter().enumerate().rev() {
        if let Message::User { .. } = msg {
            turns += 1;
        }

        if turns < 2 {
            continue;
        }

        if let Message::Assistant {
            content: Some(c), ..
        } = msg
        {
            if c.starts_with("Summary:") {
                break;
            }
        }

        if let Message::Tool { content, media, .. } = msg {
            let estimate = estimate_tokens(content) + estimate_content_tokens(media);
            total_tool_tokens += estimate;

            if total_tool_tokens > PRUNE_PROTECT_THRESHOLD {
                pruned_tokens += estimate;
                to_prune.push(i);
            }
        }
    }

    if pruned_tokens > PRUNE_MINIMUM_SAVINGS {
        for idx in to_prune {
            if let Message::Tool { content, media, .. } = &mut messages[idx] {
                *content = "[Output pruned]".to_string();
                media.clear();
            }
        }
        return pruned_tokens;
    }

    0
}

pub struct ConsolidationInput {
    pub tool_call_ids: Vec<String>,
    pub tool_names: Vec<String>,
    pub tool_contents: Vec<String>,
}

pub struct ConsolidationResult {
    pub summary_text: String,
    pub file_path: std::path::PathBuf,
    pub n_consolidated: usize,
}

pub fn build_consolidation_text(input: &ConsolidationInput) -> String {
    let mut text = String::new();
    for i in 0..input.tool_call_ids.len() {
        let name = input
            .tool_names
            .get(i)
            .map(|s| s.as_str())
            .unwrap_or("unknown");
        let content = &input.tool_contents[i];
        text.push_str(&format!("--- {}[{}] ---\n{}\n\n", name, i + 1, content));
    }
    text
}

pub fn save_consolidation_file(raw_text: &str) -> std::path::PathBuf {
    let dir = dirs::home_dir()
        .map(|h| h.join(".gewu").join("tool-consolidation"))
        .unwrap_or_else(|| std::path::PathBuf::from(CONSOLIDATION_DIR));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("Failed to create consolidation dir: {}", e);
    }
    let file_name = format!("consol_{}.txt", uuid::Uuid::new_v4());
    let file_path = dir.join(&file_name);
    if let Err(e) = std::fs::write(&file_path, raw_text) {
        tracing::warn!("Failed to write consolidation file: {}", e);
    }
    file_path
}

pub fn build_consolidation_prompt(raw_text: &str, n_results: usize) -> Vec<Message> {
    vec![
        Message::System {
            content: format!(
                "You are a data consolidation assistant. You received {} tool call results that need to be summarized into a single concise report. \
                Preserve ALL key data points: numbers, rankings, names, scores, p-values, fold enrichments, tissue/stage names. \
                Structure the summary clearly. Do NOT omit any tool's findings — every result must be represented. \
                Be thorough but eliminate redundant raw data.",
                n_results
            ),
        },
        Message::User {
            content: vec![ContentPart::Text {
                text: format!(
                    "Summarize these {} tool results:\n\n{}",
                    n_results, raw_text
                ),
            }],
        },
    ]
}

pub fn apply_consolidation_to_memory(
    messages: &mut Vec<Message>,
    tool_call_ids: &[String],
    summary: &str,
    file_path: &std::path::Path,
) {
    let id_set: std::collections::HashSet<&str> =
        tool_call_ids.iter().map(|s| s.as_str()).collect();
    let last_id = tool_call_ids.last().map(|s| s.as_str()).unwrap_or("");

    for msg in messages.iter_mut() {
        if let Message::Tool {
            tool_call_id,
            content,
            media,
            ..
        } = msg
        {
            if id_set.contains(tool_call_id.as_str()) {
                if tool_call_id == last_id {
                    *content = format!(
                        "[Consolidated summary of {} tool calls]\n\n{}\n\n[Full data: {}]",
                        tool_call_ids.len(),
                        summary,
                        file_path.display()
                    );
                } else {
                    *content = "[See consolidated summary below]".to_string();
                }
                media.clear();
            }
        }
    }
}
