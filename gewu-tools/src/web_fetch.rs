//! Web Fetch Tool
//!
//! This tool allows the AI to retrieve content from specified URLs.
//! It supports converting HTML responses into Markdown for easier consumption by the LLM.

use crate::fs_utils::into_tool_error;
use crate::response::{tool_error, tool_success};
use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use std::error::Error;
use futures_util::StreamExt;
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

const MAX_RESPONSE_BYTES: usize = 2_000_000;

fn normalize_fetch_url(input: &str) -> Result<String, String> {
    let candidate = if input.starts_with("http://") || input.starts_with("https://") {
        input.to_string()
    } else {
        format!("https://{input}")
    };
    let parsed = reqwest::Url::parse(&candidate).map_err(|error| error.to_string())?;
    if parsed.scheme() == "http" && !is_local_http_host(parsed.host_str()) {
        let mut upgraded = parsed;
        upgraded
            .set_scheme("https")
            .map_err(|_| "failed to upgrade URL scheme to https".to_string())?;
        return Ok(upgraded.to_string());
    }
    Ok(parsed.to_string())
}

fn is_local_http_host(host: Option<&str>) -> bool {
    matches!(host, Some("127.0.0.1") | Some("localhost") | Some("::1"))
}

async fn read_limited_text(response: reqwest::Response, max_bytes: usize) -> Result<String, String> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        if body.len() + chunk.len() > max_bytes {
            return Err(format!("Response exceeded {max_bytes} byte limit"));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|error| error.to_string())
}

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
        let url = match normalize_fetch_url(&input.url) {
            Ok(url) => url,
            Err(message) => {
                return Ok(tool_error(
                    "webfetch",
                    "invalid_url",
                    false,
                    message,
                    Some("Provide a valid http(s) URL or hostname.".to_string()),
                ));
            }
        };

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("opencode-cli/0.1.0")
            .build()
            .map_err(into_tool_error)?;

        let response = match client.get(&url).send().await {
            Ok(response) => response,
            Err(error) => {
                return Ok(tool_error(
                    "webfetch",
                    "request_failed",
                    true,
                    error.to_string(),
                    Some("Check the URL and network connectivity, then retry.".to_string()),
                ));
            }
        };
        let status = response.status();

        if !status.is_success() {
            return Ok(tool_error(
                "webfetch",
                "http_status",
                status.is_server_error(),
                format!("Request failed with status: {status}"),
                None,
            ));
        }

        let html = match read_limited_text(response, MAX_RESPONSE_BYTES).await {
            Ok(html) => html,
            Err(message) => {
                return Ok(tool_error(
                    "webfetch",
                    "response_too_large",
                    false,
                    message,
                    Some(format!("Fetch a smaller document (limit: {MAX_RESPONSE_BYTES} bytes).")),
                ));
            }
        };

        let content = match input.format.as_str() {
            "html" => html,
            _ => html2md::parse_html(&html),
        };

        Ok(tool_success(
            "webfetch",
            serde_json::json!({
                "url": url,
                "status": status.as_u16(),
                "format": input.format,
                "content": content,
            }),
            Vec::new(),
            Vec::new(),
        ))
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

    #[tokio::test]
    async fn web_fetch_returns_structured_success_envelope() -> Result<()> {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/structured")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body("<html><body><h1>Hello, Structured!</h1></body></html>")
            .create_async()
            .await;

        let tool = WebFetchTool;
        let args = serde_json::json!({ "url": format!("{}/structured", server.url()) });
        let ctx = ToolContext::new("test", "msg");
        let result = tool.execute(args, &ctx).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let value: serde_json::Value = serde_json::from_str(&result.output)?;

        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["status"], 200);
        assert!(value["data"]["content"].as_str().unwrap().contains("Hello, Structured!"));
        Ok(())
    }

    #[tokio::test]
    async fn web_fetch_returns_structured_error_for_non_success_status() -> Result<()> {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/missing")
            .with_status(404)
            .with_body("missing")
            .create_async()
            .await;

        let tool = WebFetchTool;
        let args = serde_json::json!({ "url": format!("{}/missing", server.url()) });
        let ctx = ToolContext::new("test", "msg");
        let result = tool.execute(args, &ctx).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let value: serde_json::Value = serde_json::from_str(&result.output)?;

        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "http_status");
        assert_eq!(value["error"]["retryable"], false);
        Ok(())
    }

    #[test]
    fn normalize_url_only_preserves_real_localhost_http() {
        assert_eq!(normalize_fetch_url("http://127.0.0.1:3210/path").unwrap(), "http://127.0.0.1:3210/path");
        assert_eq!(normalize_fetch_url("http://localhost:3210/path").unwrap(), "http://localhost:3210/path");
        assert_eq!(normalize_fetch_url("http://127.0.0.1.evil.com/path").unwrap(), "https://127.0.0.1.evil.com/path");
    }

}
