//! Search Tools
//!
//! This module implements tools for semantic code search and web search, powered by Exa (formerly Metaphor).
//!
//! - `CodeSearchTool`: Finds relevant code snippets using semantic embedding search.
//! - `WebSearchTool`: Performs general web searches to retrieve external information.

use crate::fs_utils::into_tool_error;
use anyhow::Result;
use async_trait::async_trait;
use futures_util::StreamExt;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use reqwest::Client;
use schemars::schema::RootSchema;
use serde::Deserialize;
use std::error::Error;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CodeSearchArgs {
    query: String,
    #[serde(rename = "tokensNum", default = "default_tokens")]
    tokens_num: u32,
}

fn default_tokens() -> u32 {
    5000
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WebSearchArgs {
    query: String,
    #[serde(rename = "numResults", default = "default_num_results")]
    num_results: u32,
}

fn default_num_results() -> u32 {
    8
}

pub struct CodeSearchTool;

pub struct WebSearchTool;

#[async_trait]
impl Tool for CodeSearchTool {
    fn name(&self) -> &str {
        "codesearch"
    }

    fn description(&self) -> &str {
        include_str!("prompts/codesearch.txt")
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(CodeSearchArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: CodeSearchArgs = serde_json::from_value(args).map_err(into_tool_error)?;

        let client = Client::new();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "get_code_context_exa",
                "arguments": {
                    "query": input.query,
                    "tokensNum": input.tokens_num
                }
            }
        });

        let res = client
            .post("https://mcp.exa.ai/mcp")
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(into_tool_error)?;

        if !res.status().is_success() {
            return Ok(ToolResult::new(
                "codesearch",
                format!("Request failed: {}", res.status()),
            ));
        }

        let mut stream = res.bytes_stream();
        let mut buffer = String::new();
        let mut results = String::new();

        while let Some(item) = stream.next().await {
            let chunk = item.map_err(into_tool_error)?;
            let text = String::from_utf8_lossy(&chunk);
            buffer.push_str(&text);

            while let Some(idx) = buffer.find('\n') {
                let line = buffer[..idx].to_string();
                buffer = buffer[idx + 1..].to_string();

                if let Some(data_str) = line.strip_prefix("data: ")
                    && let Ok(val) = serde_json::from_str::<serde_json::Value>(data_str)
                    && let Some(content) = val
                        .get("result")
                        .and_then(|r| r.get("content"))
                        .and_then(|c| c.as_array())
                {
                    for item in content {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            if !results.is_empty() {
                                results.push_str("\n\n---\n\n");
                            }
                            results.push_str(text);
                        }
                    }
                }
            }
        }

        if results.is_empty() {
            results = "No results found.".to_string();
        }

        Ok(ToolResult::new("codesearch", results))
    }
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

        let client = Client::new();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "web_search_exa",
                "arguments": {
                    "query": input.query,
                    "numResults": input.num_results,
                    "type": "auto",
                    "livecrawl": "fallback"
                }
            }
        });

        let res = client
            .post("https://mcp.exa.ai/mcp")
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(into_tool_error)?;

        if !res.status().is_success() {
            return Ok(ToolResult::new(
                "websearch",
                format!("Request failed: {}", res.status()),
            ));
        }

        let mut stream = res.bytes_stream();
        let mut buffer = String::new();
        let mut results = String::new();

        while let Some(item) = stream.next().await {
            let chunk = item.map_err(into_tool_error)?;
            let text = String::from_utf8_lossy(&chunk);
            buffer.push_str(&text);

            while let Some(idx) = buffer.find('\n') {
                let line = buffer[..idx].to_string();
                buffer = buffer[idx + 1..].to_string();

                if let Some(data_str) = line.strip_prefix("data: ")
                    && let Ok(val) = serde_json::from_str::<serde_json::Value>(data_str)
                    && let Some(content) = val
                        .get("result")
                        .and_then(|r| r.get("content"))
                        .and_then(|c| c.as_array())
                {
                    for item in content {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            if !results.is_empty() {
                                results.push_str("\n\n---\n\n");
                            }
                            results.push_str(text);
                        }
                    }
                }
            }
        }

        if results.is_empty() {
            results = "No results found.".to_string();
        }

        Ok(ToolResult::new("websearch", results))
    }
}
