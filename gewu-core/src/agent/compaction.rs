//! Agent-side compaction logic.
//!
//! Implements opencode V2-style compaction: when history overflows the
//! context limit, everything older than a small verbatim tail is summarized
//! by the LLM (tool outputs truncated in the summarizer's view), and history
//! gains a durable checkpoint carrying the summary plus the serialized recent
//! tail. Older messages are excluded from model context, never deleted.

use crate::compaction::{
    self, KEEP_TOKENS, SUMMARY_OUTPUT_TOKENS, build_summary_prompt, compaction_body, is_overflow,
    select_recent_tail, serialize_messages,
};
use crate::types::{ContentPart, Message, ProviderEvent};
use anyhow::{Result, anyhow};
use futures::StreamExt;
use tracing::{info, warn};

use super::Agent;

struct CompactionPlan {
    summary_request: Vec<Message>,
    recent: String,
}

#[derive(Debug, PartialEq)]
enum CompactionSkipReason {
    NoHistoryToSummarize,
    PromptWouldNotFit,
}

fn plan_compaction(
    messages: &[Message],
    context_limit: usize,
) -> Result<CompactionPlan, CompactionSkipReason> {
    let (prior_checkpoint, body) = compaction_body(messages);
    let split = select_recent_tail(body, KEEP_TOKENS);
    let head = &body[..split];
    let recent = &body[split..];
    let prior_recent = prior_checkpoint.map(|(_, recent)| recent);
    if head.is_empty() && prior_recent.as_ref().is_none_or(|r| r.is_empty()) {
        return Err(CompactionSkipReason::NoHistoryToSummarize);
    }
    let mut context = Vec::new();
    if let Some(recent) = &prior_recent
        && !recent.is_empty()
    {
        context.push((*recent).to_owned());
    }
    context.push(serialize_messages(head));
    let prompt = build_summary_prompt(prior_checkpoint.map(|(summary, _)| summary), &context);
    if compaction::estimate_tokens(&prompt) + SUMMARY_OUTPUT_TOKENS > context_limit {
        return Err(CompactionSkipReason::PromptWouldNotFit);
    }
    Ok(CompactionPlan {
        summary_request: vec![Message::User {
            content: vec![ContentPart::Text { text: prompt }],
        }],
        recent: serialize_messages(recent),
    })
}

/// Error type for compaction failures.
#[derive(Debug)]
pub struct CompactionError {
    /// Human-readable error message.
    pub message: String,
}

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CompactionError: {}", self.message)
    }
}

impl std::error::Error for CompactionError {}

impl Agent {
    #[cfg(test)]
    pub(super) async fn compact_if_needed(&mut self) -> Result<bool> {
        let total_tokens = compaction::active_context(self.memory.messages()).estimate_tokens();
        self.compact_with_estimate(total_tokens).await
    }

    pub(super) async fn compact_with_estimate(&mut self, total_tokens: usize) -> Result<bool> {
        if !is_overflow(total_tokens, self.context_limit) {
            return Ok(false);
        }

        info!("Compacting history (total tokens: {})", total_tokens);

        // Body = everything the summarizer may touch: after the system
        // prompt, and after any prior checkpoint (whose summary/recent
        // already represent the older history).
        let plan = match plan_compaction(self.memory.messages(), self.context_limit) {
            Ok(plan) => plan,
            Err(CompactionSkipReason::NoHistoryToSummarize) => {
                warn!("Compaction skipped: no history older than the kept tail");
                return Ok(false);
            }
            Err(CompactionSkipReason::PromptWouldNotFit) => {
                warn!("Compaction skipped: summary prompt would not fit the context window");
                return Ok(false);
            }
        };
        let provider = self.llm.clone();
        let mut stream = provider
            .stream_response(&plan.summary_request, &[], self.log.as_ref())
            .await
            .map_err(|e| anyhow!("Compaction failed: {}", e))?;

        let mut summary_text = String::new();
        while let Some(event_result) = stream.next().await {
            let event = event_result.map_err(|e| anyhow!("Compaction stream error: {}", e))?;
            match event {
                ProviderEvent::ContentDelta { delta } => summary_text.push_str(&delta),
                // The compaction request is a real LLM request; its usage
                // counts toward the session's provider-reported totals.
                ProviderEvent::Usage { usage } => self.provider_usage.add(&usage),
                ProviderEvent::Error { error } => {
                    return Err(anyhow!("Compaction failed: {error}"));
                }
                _ => {}
            }
        }

        if summary_text.trim().is_empty() {
            // Returning an error (instead of silently continuing) keeps the
            // loop from retrying the expensive summary on every iteration.
            return Err(anyhow!("Compaction failed: LLM returned an empty summary"));
        }

        self.memory
            .append_compaction(summary_text.trim().to_string(), plan.recent)
            .await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_distinguishes_skip_reasons() {
        assert!(matches!(
            plan_compaction(&[], 100_000),
            Err(CompactionSkipReason::NoHistoryToSummarize)
        ));
        let history = vec![Message::User {
            content: vec![ContentPart::Text {
                text: "x".repeat(100_000),
            }],
        }];
        assert!(matches!(
            plan_compaction(&history, 1),
            Err(CompactionSkipReason::PromptWouldNotFit)
        ));
    }

    #[test]
    fn planner_returns_request_and_recent_tail() {
        let history = vec![
            Message::User {
                content: vec![ContentPart::Text {
                    text: "x".repeat(40_000),
                }],
            },
            Message::User {
                content: vec![ContentPart::Text {
                    text: "recent".into(),
                }],
            },
        ];
        let plan = plan_compaction(&history, 100_000).expect("plan");
        assert_eq!(plan.recent, "[User]: recent");
        assert!(
            matches!(&plan.summary_request[0], Message::User { content } if matches!(&content[0], ContentPart::Text { text } if text.contains(&"x".repeat(40_000))))
        );
    }
}
