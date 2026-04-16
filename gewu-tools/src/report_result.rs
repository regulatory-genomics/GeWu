use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::error::Error;

/// Tool that subagents call to report their task outcome.
///
/// The agent calls this at the end of execution to declare whether the
/// task succeeded or is blocked. TaskTool listens for the `report_result`
/// tool-finish event and uses the `status` field to drive task state.
pub struct ReportResultTool;

#[derive(Debug, Deserialize)]
pub struct ReportResultArgs {
    pub status: String,
    #[serde(default)]
    pub block_reason: Option<String>,
    #[serde(default)]
    pub result: Option<Value>,
}

#[async_trait]
impl Tool for ReportResultTool {
    fn name(&self) -> &str {
        "report_result"
    }

    fn description(&self) -> &str {
        "Report the outcome of your task. Call this exactly once when you are done. \
         Set status to \"succeeded\" if you completed the task, or \"blocked\" if \
         you could not complete it due to a missing precondition."
    }

    fn schema(&self) -> RootSchema {
        serde_json::from_value(json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["succeeded", "blocked"],
                    "description": "Whether the task succeeded or is blocked"
                },
                "block_reason": {
                    "type": "string",
                    "description": "Why the task is blocked (only when status is blocked)"
                },
                "result": {
                    "description": "The task result or output (when succeeded)"
                }
            },
            "required": ["status"]
        }))
        .unwrap()
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: ReportResultArgs = serde_json::from_value(args)?;
        // Echo back the structured report so TaskTool can parse it from ToolFinish output
        let output = json!({
            "status": input.status,
            "block_reason": input.block_reason,
            "result": input.result,
        });
        Ok(ToolResult::new("report_result", output.to_string()))
    }
}
