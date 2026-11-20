//! Search Tools
//!
//! This module implements tools for web search, powered by Exa (formerly Metaphor).
//!
//! - `WebSearchTool`: Performs general web searches to retrieve external information.

use crate::fs_utils::into_tool_error;
use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use reqwest::Client;
use schemars::schema::RootSchema;
use serde::Deserialize;
use std::error::Error;
use std::time::Duration;

const EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WebSearchArgs {
    query: String,
    objective: Option<String>,
    #[serde(rename = "numResults", default = "default_num_results")]
    num_results: u32,
}

fn default_num_results() -> u32 {
    8
}

pub struct WebSearchTool;

fn extract_mcp_content(value: &serde_json::Value) -> Result<Option<Vec<String>>, String> {
    if let Some(error) = value.get("error") {
        let code = error.get("code").and_then(|v| v.as_i64());
        let message = error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown MCP error");
        return Err(match code {
            Some(code) => format!("MCP error {}: {}", code, message),
            None => format!("MCP error: {}", message),
        });
    }

    let Some(result) = value.get("result") else {
        return Ok(None);
    };

    let texts = result
        .get("content")
        .and_then(|content| content.as_array())
        .map(|content| {
            content
                .iter()
                .filter_map(|item| item.get("text").and_then(|text| text.as_str()))
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let is_error = result
        .get("isError")
        .or_else(|| result.get("is_error"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if is_error {
        let message = if texts.is_empty() {
            "MCP tool execution failed".to_string()
        } else {
            texts.join("\n")
        };
        return Err(message);
    }

    Ok(Some(texts))
}

fn parse_mcp_response(body: &str) -> Result<String, String> {
    let normalized = body.replace("\r\n", "\n");
    let mut messages = Vec::new();

    for event in normalized.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        if !data.is_empty() && data != "[DONE]" {
            messages.push(data);
        }
    }

    if messages.is_empty() && !body.trim().is_empty() {
        messages.push(body.trim().to_string());
    }

    let mut results = Vec::new();
    let mut saw_result = false;
    for message in messages {
        let value: serde_json::Value = serde_json::from_str(&message)
            .map_err(|error| format!("Invalid MCP response: {}", error))?;
        if let Some(texts) = extract_mcp_content(&value)? {
            saw_result = true;
            results.extend(texts);
        }
    }

    if !saw_result {
        return Err("MCP response did not contain a tool result".to_string());
    }
    if results.is_empty() {
        return Ok("No results found.".to_string());
    }

    Ok(results.join("\n\n---\n\n"))
}

async fn call_exa_tool(
    title: &str,
    tool_name: &str,
    arguments: serde_json::Value,
) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(into_tool_error)?;
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": arguments
        }
    });

    let response = client
        .post(EXA_MCP_URL)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(into_tool_error)?;
    let status = response.status();
    let response_body = response.text().await.map_err(into_tool_error)?;

    if !status.is_success() {
        return Err(into_tool_error(std::io::Error::other(format!(
            "Exa request failed with {}: {}",
            status,
            response_body.trim()
        ))));
    }

    let output = parse_mcp_response(&response_body)
        .map_err(|message| into_tool_error(std::io::Error::other(message)))?;
    Ok(ToolResult::new(title, output))
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "websearch"
    }

    fn description(&self) -> &str {
        include_str!("prompts/websearch.txt")
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(WebSearchArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: WebSearchArgs = serde_json::from_value(args).map_err(into_tool_error)?;
        let objective = input.objective.unwrap_or_else(|| input.query.clone());

        call_exa_tool(
            "websearch",
            "web_search_exa",
            serde_json::json!({
                "query": input.query,
                "objective": objective,
                "numResults": input.num_results
            }),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::parse_mcp_response;

    #[test]
    fn parses_json_response() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"result one"}]}}"#;

        assert_eq!(parse_mcp_response(body).unwrap(), "result one");
    }

    #[test]
    fn parses_multiline_sse_data() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\ndata: \"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"result two\"}]}}\n\n";

        assert_eq!(parse_mcp_response(body).unwrap(), "result two");
    }

    #[test]
    fn reports_json_rpc_errors() {
        let body = r#"data: {"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Invalid arguments"}}

"#;

        let error = parse_mcp_response(body).unwrap_err();
        assert!(error.contains("Invalid arguments"));
    }

    #[test]
    fn reports_mcp_tool_errors() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[{"type":"text","text":"Rate limited"}]}}"#;

        assert_eq!(parse_mcp_response(body).unwrap_err(), "Rate limited");
    }
}
