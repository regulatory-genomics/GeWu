use gewu_core::types::ToolResult;
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Clone, Serialize)]
pub struct ToolErrorBody {
    pub code: String,
    pub retryable: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_fix: Option<String>,
}

pub fn tool_success(
    tool_name: impl Into<String>,
    data: serde_json::Value,
    warnings: Vec<String>,
    next_actions: Vec<String>,
) -> ToolResult {
    let tool_name = tool_name.into();
    let output = json!({
        "ok": true,
        "data": data,
        "warnings": warnings,
        "next_actions": next_actions,
    });
    ToolResult::new(&tool_name, output.to_string())
}

pub fn tool_error(
    tool_name: impl Into<String>,
    code: impl Into<String>,
    retryable: bool,
    message: impl Into<String>,
    suggested_fix: Option<String>,
) -> ToolResult {
    let tool_name = tool_name.into();
    let error = ToolErrorBody {
        code: code.into(),
        retryable,
        message: message.into(),
        suggested_fix,
    };
    let output = json!({
        "ok": false,
        "error": error,
    });
    ToolResult::new(&tool_name, output.to_string())
}
