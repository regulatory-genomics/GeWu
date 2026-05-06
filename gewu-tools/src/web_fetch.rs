//! Web Fetch Tool
//!
//! This tool allows the AI to retrieve content from specified URLs.
//! It supports converting HTML responses into Markdown for easier consumption by the LLM.

use crate::fs_utils::into_tool_error;
use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::time::Duration;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WebFetchArgs {
    pub url: String,
    #[serde(default = "default_format")]
    pub format: String,
}

fn default_format() -> String {
    "markdown".to_string()
}

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "webfetch"
    }

    fn description(&self) -> &str {
        include_str!("prompts/webfetch.txt")
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(WebFetchArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: WebFetchArgs = serde_json::from_value(args).map_err(into_tool_error)?;
        let mut url = input.url;

        if url.starts_with("http://") && !url.contains("127.0.0.1") && !url.contains("localhost") {
            url = url.replacen("http://", "https://", 1);
        } else if !url.starts_with("http://") && !url.starts_with("https://") {
            url = format!("https://{}", url);
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("opencode-cli/0.1.0")
            .build()
            .map_err(into_tool_error)?;

        let response = client.get(&url).send().await.map_err(into_tool_error)?;
        let status = response.status();

        if !status.is_success() {
            return Ok(ToolResult::new(
                "webfetch",
                format!("Request failed with status: {}", status),
            ));
        }

        let html = response.text().await.map_err(into_tool_error)?;

        let output = match input.format.as_str() {
            "html" => html,
            _ => html2md::parse_html(&html),
        };

        Ok(ToolResult::new("webfetch", output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use mockito;

    #[tokio::test]
    async fn test_web_fetch_tool() -> Result<()> {
        let mut server = mockito::Server::new_async().await;

        let _m = server
            .mock("GET", "/hello")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body("<html><body><h1>Hello, World!</h1></body></html>")
            .create_async()
            .await;

        let tool = WebFetchTool;
        let url = format!("{}/hello", server.url());

        let args = serde_json::json!({ "url": url });
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        assert!(result.output.contains("Hello, World!"));
        assert!(!result.output.contains("<h1>"));
        Ok(())
    }
}
