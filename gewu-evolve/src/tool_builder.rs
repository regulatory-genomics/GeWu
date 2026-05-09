//! Tool builder for generating new tools from natural language descriptions.

use crate::sandbox::PythonSandbox;
use crate::skill_library::{Skill, SkillImplementation};
use anyhow::{Result, Context};
use futures::StreamExt;
use gewu_core::traits::LlmProvider;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedTool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResult {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

pub struct ToolBuilder;

impl ToolBuilder {
    pub async fn generate(
        description: &str,
        llm: &dyn LlmProvider,
    ) -> Result<GeneratedTool> {
        let prompt = Self::build_generation_prompt(description);

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
                    _ => {}
                },
                Err(e) => {
                    tracing::warn!("Error in tool builder stream: {}", e);
                    break;
                }
            }
        }

        Self::parse_generation_response(&response_text)
    }

    fn build_generation_prompt(description: &str) -> String {
        format!(
            r#"Generate a Python tool based on the following description:

{}

Your task is to generate a tool that can be executed by an AI agent. The tool should:
1. Have a clear, descriptive name (snake_case)
2. Have a JSON Schema for its parameters
3. Contain Python code that implements the functionality
4. Store its final result in a variable called 'result'

Respond ONLY with valid JSON in this exact format:
{{
  "name": "tool_name",
  "description": "What this tool does",
  "parameters": {{
    "type": "object",
    "properties": {{
      "param1": {{
        "type": "string",
        "description": "Description of param1"
      }}
    }},
    "required": ["param1"]
  }},
  "code": "def main(args):\n    # Your code here\n    result = ...\n    return result"
}}"#,
            description
        )
    }

    fn parse_generation_response(response: &str) -> Result<GeneratedTool> {
        let trimmed = response.trim();

        let json_start = trimmed.find('{');
        let json_end = trimmed.rfind('}');

        if json_start.is_none() || json_end.is_none() {
            anyhow::bail!("No JSON found in response: {}", response);
        }

        let json_str = &trimmed[json_start.unwrap()..=json_end.unwrap()];

        #[derive(Deserialize)]
        struct RawTool {
            name: String,
            description: String,
            parameters: serde_json::Value,
            code: String,
        }

        let raw = serde_json::from_str::<RawTool>(json_str)
            .context("Failed to parse tool generation response")?;

        Ok(GeneratedTool {
            name: raw.name,
            description: raw.description,
            parameters: raw.parameters,
            code: raw.code,
        })
    }

    pub async fn validate(skill: &Skill, sandbox: &PythonSandbox) -> Result<ValidationResult> {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        if skill.implementation.language != "python" {
            errors.push(format!(
                "Unsupported language: {}",
                skill.implementation.language
            ));
            return Ok(ValidationResult {
                valid: false,
                errors,
                warnings,
            });
        }

        let code = &skill.implementation.code;
        if !code.contains("def main") && !code.contains("result") {
            warnings.push(
                "Code should define a 'main' function and set 'result' variable".to_string(),
            );
        }

        let test_code = format!(
            "{}\nresult = main({{}})",
            code
        );

        let test_result = sandbox.execute(&test_code, serde_json::json!({})).await?;

        if test_result.timed_out {
            errors.push("Code execution timed out".to_string());
        } else if let Some(err) = test_result.error {
            errors.push(format!("Execution error: {}", err));
        }

        Ok(ValidationResult {
            valid: errors.is_empty(),
            errors,
            warnings,
        })
    }

    pub fn to_skill(generated: &GeneratedTool) -> Skill {
        Skill::new(
            generated.name.clone(),
            generated.description.clone(),
            generated.parameters.clone(),
            SkillImplementation::python(generated.code.clone()),
        )
    }
}

impl Default for ToolBuilder {
    fn default() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_generation_response() {
        let response = r#"Some text before
{
  "name": "parse_csv",
  "description": "Parse a CSV file",
  "parameters": {
    "type": "object",
    "properties": {
      "path": {
        "type": "string"
      }
    }
  },
  "code": "def main(args):\n    result = 'parsed'"
}
More text after"#;

        let result = ToolBuilder::parse_generation_response(response);
        assert!(result.is_ok());
        let tool = result.unwrap();
        assert_eq!(tool.name, "parse_csv");
        assert_eq!(tool.description, "Parse a CSV file");
    }
}
