//! Reviewer agent for plan evaluation and failure analysis.

use crate::critique::CritiqueResult;
use anyhow::Result;
use futures::StreamExt;
use gewu_core::Agent;

#[derive(Debug, Clone)]
pub struct ReviewResult {
    pub approved: bool,
    pub issues: Vec<String>,
    pub suggestions: Vec<String>,
}

pub struct Reviewer {
    agent: Agent,
}

impl Reviewer {
    pub fn new(agent: Agent) -> Self {
        Self { agent }
    }

    pub async fn review_plan(&mut self, task: &str, plan: &str) -> Result<String> {
        let prompt = Self::build_review_prompt(task, plan);
        self.agent.reset().await;
        self.collect_response(&prompt).await
    }

    async fn collect_response(&mut self, prompt: &str) -> Result<String> {
        let mut stream = Box::pin(self.agent.ask(prompt));
        let mut response_text = String::new();

        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => match event {
                    gewu_core::types::AgentEvent::ContentDelta { delta } => {
                        response_text.push_str(&delta);
                    }
                    gewu_core::types::AgentEvent::Finish { .. } => break,
                    _ => {}
                },
                Err(e) => {
                    tracing::warn!("Error in review stream: {}", e);
                    break;
                }
            }
        }

        Ok(response_text)
    }

    fn build_review_prompt(task: &str, plan: &str) -> String {
        format!(
            r#"You are a reviewer agent evaluating execution plans.

When reviewing, you may use available tools to:
- Read files to verify plan feasibility
- Search code to check for potential issues
- Look up documentation

However, you should NOT execute bash commands or modify files.

Please evaluate this plan and respond with:

First, state clearly whether this plan is approved:
Approved: yes or no

If not approved, provide:
Issues:
- issue 1
- issue 2
- etc.

Suggestions for improvement:
- suggestion 1
- suggestion 2
- etc.

Be specific and constructive in your feedback.

Task:
{}

Plan:
{}"#,
            task, plan
        )
    }

    pub async fn critique_failure(
        &mut self,
        task: &str,
        plan: &str,
        error: &str,
        tools_used: &[String],
    ) -> Result<String> {
        let prompt = Self::build_critique_prompt(task, plan, error, tools_used);
        self.agent.reset().await;
        self.collect_response(&prompt).await
    }

    fn build_critique_prompt(
        task: &str,
        plan: &str,
        error: &str,
        tools_used: &[String],
    ) -> String {
        let tools_str = if tools_used.is_empty() {
            "No tools were used".to_string()
        } else {
            tools_used.join(", ")
        };

        format!(
            r#"You are analyzing why a task failed during execution.

When analyzing, you may use available tools to:
- Read files to investigate the failure
- Search code for potential issues
- Look up documentation

However, you should NOT execute bash commands or modify files.

Task Description:
{}

Plan that was executed:
{}

Error Message:
{}

Tools Used:
{}

Please analyze and respond with:

Root cause: Explain specifically what went wrong and why.

Suggested fix: Describe how the agent should approach this differently. Be actionable and specific.

Should retry: yes or no

Tools to adjust: (optional) list any tools that might need different usage"#,
            task, plan, error, tools_str
        )
    }

    pub fn parse_review_response(response: &str) -> ReviewResult {
        let response_lower = response.to_lowercase();

        let approved = response_lower.contains("approved: yes")
            || response_lower.contains("approved: yep")
            || response_lower.contains("approved: yeah")
            || response_lower.contains("approved: true");

        let issues = Self::extract_section(response, "Issues:");
        let suggestions = Self::extract_section(response, "Suggestions:");

        ReviewResult {
            approved,
            issues,
            suggestions,
        }
    }

    fn extract_section(text: &str, header: &str) -> Vec<String> {
        let header_lower = header.to_lowercase();
        let text_lower = text.to_lowercase();

        if let Some(pos) = text_lower.find(&header_lower) {
            let start = pos + header.len();
            let rest = &text[start..];

            let end_markers = ["\n\n", "\nIssues:", "\nSuggestions:", "\nRoot cause:", "\nShould retry:"];
            let mut end = rest.len();
            for marker in &end_markers {
                if let Some(marker_pos) = rest.to_lowercase().find(&marker.to_lowercase()) {
                    if marker_pos < end {
                        end = marker_pos;
                    }
                }
            }

            rest[..end]
                .lines()
                .filter(|line| {
                    let trimmed = line.trim();
                    trimmed.starts_with('-') || trimmed.starts_with('*') || trimmed.starts_with("•")
                })
                .map(|line| line.trim_start_matches(|c: char| c == '-' || c == '*' || c == '•' || c == ' ').to_string())
                .filter(|s| !s.is_empty())
                .collect()
        } else {
            vec![]
        }
    }

    pub fn parse_critique_response(response: &str) -> CritiqueResult {
        let response_lower = response.to_lowercase();

        let root_cause = Self::extract_field(response, "Root cause:")
            .unwrap_or_else(|| "Unable to determine root cause".to_string());

        let suggested_fix = Self::extract_field(response, "Suggested fix:")
            .unwrap_or_else(|| response.to_string());

        let should_retry = response_lower.contains("should retry: yes")
            || response_lower.contains("should retry: yep")
            || response_lower.contains("should retry: yeah")
            || response_lower.contains("should retry: true");

        let tools_str = Self::extract_field(response, "Tools to adjust:")
            .unwrap_or_default();

        let tools_to_adjust: Vec<String> = tools_str
            .split(|c| c == ',' || c == '\n')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty() && s != "none" && s != "n/a")
            .collect();

        CritiqueResult {
            root_cause,
            suggested_fix,
            tools_to_adjust,
            should_retry,
        }
    }

    fn extract_field(text: &str, field: &str) -> Option<String> {
        let field_lower = field.to_lowercase();
        let text_lower = text.to_lowercase();

        if let Some(pos) = text_lower.find(&field_lower) {
            let start = pos + field.len();
            let rest = &text[start..].trim_start();

            let end_markers = ["\n\n", "\nRoot cause:", "\nSuggested fix:", "\nShould retry:", "\nTools to adjust:"];
            let mut end = rest.len();
            for marker in &end_markers {
                if let Some(marker_pos) = rest.to_lowercase().find(&marker.to_lowercase()) {
                    if marker_pos < end {
                        end = marker_pos;
                    }
                }
            }

            let value = &rest[..end];
            if value.is_empty() {
                None
            } else {
                Some(value.trim().to_string())
            }
        } else {
            None
        }
    }
}