//! Self-critique module for analyzing failures and generating feedback.

use anyhow::Result;
use futures::StreamExt;
use gewu_core::traits::LlmProvider;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CritiqueResult {
    pub root_cause: String,
    pub suggested_fix: String,
    pub tools_to_adjust: Vec<String>,
    pub should_retry: bool,
}

pub struct SelfCritique;

impl SelfCritique {
    pub async fn analyze(
        task_description: &str,
        error_message: &str,
        tools_used: &[String],
        llm: &dyn LlmProvider,
    ) -> Result<CritiqueResult> {
        let prompt = Self::build_critique_prompt(task_description, error_message, tools_used);

        let messages = vec![gewu_core::types::Message::User {
            content: vec![gewu_core::types::ContentPart::Text { text: prompt }],
        }];

        let mut stream = llm.stream_response(&messages, &[]).await
            .map_err(|e| anyhow::anyhow!("LLM stream error: {}", e))?;

        let mut response_text = String::new();
        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => match event {
                    gewu_core::types::ProviderEvent::ContentDelta { delta } => {
                        response_text.push_str(&delta);
                    }
                    gewu_core::types::ProviderEvent::ReasoningDelta { delta } => {
                        response_text.push_str(&delta);
                    }
                    _ => {}
                },
                Err(e) => {
                    tracing::warn!("Error in critique stream: {}", e);
                    break;
                }
            }
        }

        Self::parse_critique_response(&response_text)
    }

    fn build_critique_prompt(task_description: &str, error_message: &str, tools_used: &[String]) -> String {
        let tools_str = if tools_used.is_empty() {
            "No tools were used".to_string()
        } else {
            tools_used.join(", ")
        };

        format!(
            r#"You are analyzing why a task failed. Your goal is to help identify the root cause and propose a fix.

Task Description:
{}

Error Message:
{}

Tools Used:
{}

Please analyze and respond with a JSON object containing:
1. "root_cause": Why do you think this failed? Be specific.
2. "suggested_fix": How should the agent approach this differently? Be actionable.
3. "tools_to_adjust": Which tools might need different usage or parameters? (list of tool names or empty array)
4. "should_retry": Should the agent try again with the suggested fix? (true/false)

Respond ONLY with valid JSON in this exact format:
{{
  "root_cause": "...",
  "suggested_fix": "...",
  "tools_to_adjust": ["tool1", "tool2"],
  "should_retry": true/false
}}"#,
            task_description, error_message, tools_str
        )
    }

    fn parse_critique_response(response: &str) -> Result<CritiqueResult> {
        let trimmed = response.trim();

        let json_start = trimmed.find('{');
        let json_end = trimmed.rfind('}');

        if json_start.is_none() || json_end.is_none() {
            return Ok(CritiqueResult {
                root_cause: "Unable to parse critique response".to_string(),
                suggested_fix: trimmed.to_string(),
                tools_to_adjust: Vec::new(),
                should_retry: false,
            });
        }

        let json_str = &trimmed[json_start.unwrap()..=json_end.unwrap()];

        #[derive(Deserialize)]
        struct RawCritique {
            root_cause: String,
            suggested_fix: String,
            tools_to_adjust: Vec<String>,
            should_retry: bool,
        }

        match serde_json::from_str::<RawCritique>(json_str) {
            Ok(raw) => Ok(CritiqueResult {
                root_cause: raw.root_cause,
                suggested_fix: raw.suggested_fix,
                tools_to_adjust: raw.tools_to_adjust,
                should_retry: raw.should_retry,
            }),
            Err(e) => {
                tracing::warn!("Failed to parse critique JSON: {}. Response: {}", e, json_str);
                Ok(CritiqueResult {
                    root_cause: "Parse error".to_string(),
                    suggested_fix: response.to_string(),
                    tools_to_adjust: Vec::new(),
                    should_retry: false,
                })
            }
        }
    }
}

impl Default for SelfCritique {
    fn default() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_critique() {
        let response = r#"Some reasoning text before
{
  "root_cause": "File path was incorrect",
  "suggested_fix": "Use absolute path instead of relative",
  "tools_to_adjust": ["read_file"],
  "should_retry": true
}
More text after"#;

        let result = SelfCritique::parse_critique_response(response).unwrap();
        assert!(result.root_cause.contains("File path"));
        assert!(result.suggested_fix.contains("absolute path"));
        assert_eq!(result.tools_to_adjust, vec!["read_file"]);
        assert!(result.should_retry);
    }

    #[test]
    fn test_parse_invalid_critique() {
        let response = "No JSON here";
        let result = SelfCritique::parse_critique_response(response).unwrap();
        assert!(result.root_cause.contains("Unable to parse"));
        assert!(!result.should_retry);
    }
}
