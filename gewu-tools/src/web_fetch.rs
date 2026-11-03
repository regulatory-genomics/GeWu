//! Web Fetch Tool
//!
//! This tool allows the AI to retrieve content from specified URLs.
//! It supports converting HTML responses into Markdown for easier consumption by the LLM.
//!
//! # Destination policy
//!
//! A fetch is a server-side request: the model chooses the address and this
//! process makes the connection. Left unchecked, that reaches loopback and
//! private networks the caller cannot reach directly (SSRF). [`WebFetchPolicy`]
//! is the switch for that check; it is off by default so existing callers keep
//! their behaviour.
//!
//! With `block_private_networks` enabled, `execute` resolves the host, refuses
//! the request when any resolved address is private, loopback, link-local,
//! unspecified, multicast or CGNAT, pins the connection to the validated
//! address, and does not follow redirects. Refusing on *any* private address is
//! deliberate: a host that mixes public and private answers is not trustworthy.

use crate::fs_utils::into_tool_error;
use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
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

/// Where `webfetch` is allowed to go.
#[derive(Debug, Clone, Default)]
pub struct WebFetchPolicy {
    /// Refuse destinations that resolve to a private address and do not follow
    /// redirects. Off by default, matching the historical behaviour.
    pub block_private_networks: bool,
    /// Host suffixes that may be fetched. Empty means "no host allowlist".
    ///
    /// This narrows, it never widens: an allowlisted host still has to pass the
    /// private-network check when that check is enabled.
    pub allowed_hosts: Vec<String>,
}

impl WebFetchPolicy {
    /// Normalize a comma-separated host list: trim, lowercase, drop a leading
    /// `*.` and trailing dots, drop empty entries.
    pub fn parse_hosts(raw: &str) -> Vec<String> {
        raw.split(',')
            .map(|host| {
                host.trim()
                    .trim_start_matches("*.")
                    .trim_end_matches('.')
                    .to_ascii_lowercase()
            })
            .filter(|host| !host.is_empty())
            .collect()
    }

    fn permits_host(&self, host: &str) -> bool {
        self.allowed_hosts.is_empty()
            || self
                .allowed_hosts
                .iter()
                .any(|allowed| host == allowed || host.ends_with(&format!(".{allowed}")))
    }
}

pub struct WebFetchTool {
    policy: Arc<WebFetchPolicy>,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new(WebFetchPolicy::default())
    }
}

impl WebFetchTool {
    pub fn new(policy: WebFetchPolicy) -> Self {
        Self {
            policy: Arc::new(policy),
        }
    }

    /// Share one policy across the tools an agent (and its sub-agents) build.
    pub fn with_shared_policy(policy: Arc<WebFetchPolicy>) -> Self {
        Self { policy }
    }
}

/// Normalize the URL: keep `http://` for loopback (a local docs server is a
/// normal development target), upgrade every other cleartext URL to https, and
/// assume https when the scheme is missing.
fn normalize_url(raw: &str) -> String {
    if raw.starts_with("http://") && !raw.contains("127.0.0.1") && !raw.contains("localhost") {
        raw.replacen("http://", "https://", 1)
    } else if !raw.starts_with("http://") && !raw.starts_with("https://") {
        format!("https://{raw}")
    } else {
        raw.to_string()
    }
}

/// True when the address is safe to reach from a public server.
fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !is_private_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return !is_private_v4(v4);
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_multicast())
        }
    }
}

fn is_private_v4(v4: Ipv4Addr) -> bool {
    let octets = v4.octets();
    v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_unspecified()
        || v4.is_multicast()
        // 100.64.0.0/10, carrier-grade NAT.
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
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
        let url_text = normalize_url(&input.url);
        let url = reqwest::Url::parse(&url_text)
            .map_err(|error| format!("webfetch: {url_text} is not a URL ({error})"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("webfetch refused: scheme {} is not http or https", url.scheme()).into());
        }
        let host = url
            .host_str()
            .ok_or_else(|| format!("webfetch refused: {url_text} has no host"))?
            .to_ascii_lowercase();
        if !self.policy.permits_host(&host) {
            return Err(format!(
                "webfetch refused: {host} is not in the allowed hosts list"
            )
            .into());
        }

        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("opencode-cli/0.1.0");

        if self.policy.block_private_networks {
            let port = url.port_or_known_default().unwrap_or(443);
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
                .await
                .map_err(|error| format!("webfetch refused: cannot resolve {host}: {error}"))?
                .collect();
            if addrs.is_empty() {
                return Err(format!("webfetch refused: {host} did not resolve").into());
            }
            if let Some(private) = addrs.iter().find(|addr| !is_public_ip(addr.ip())) {
                return Err(format!(
                    "webfetch refused: {host} resolves to {}, which is a private address",
                    private.ip()
                )
                .into());
            }
            // Pin the connection to the address that was checked, and never
            // follow a redirect: a redirect target is not re-checked.
            builder = builder
                .resolve(&host, addrs[0])
                .redirect(reqwest::redirect::Policy::none());
        }

        let client = builder.build().map_err(into_tool_error)?;

        let response = client.get(url.as_str()).send().await.map_err(into_tool_error)?;
        let status = response.status();

        if self.policy.block_private_networks && status.is_redirection() {
            return Ok(ToolResult::new(
                "webfetch",
                format!(
                    "Request redirected with status {status}; redirects are not followed while private networks are blocked."
                ),
            ));
        }

        if !status.is_success() {
            return Ok(ToolResult::new(
                "webfetch",
                format!("Request failed with status: {}", status),
            ));
        }

        let html = response.text().await.map_err(into_tool_error)?;

        let output = match input.format.as_str() {
            "html" => html,
            _ => {
                let markdown = html2md::parse_html(&html);
                markdown
            }
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

        // The default policy is permissive, so a loopback mock server still
        // works exactly as before.
        let tool = WebFetchTool::default();
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
    async fn a_private_destination_is_refused_before_any_request() {
        let mut server = mockito::Server::new_async().await;
        // No mock is registered: reaching the server at all would fail the test
        // differently, and the point is that no request is sent.
        let tool = WebFetchTool::new(WebFetchPolicy {
            block_private_networks: true,
            allowed_hosts: Vec::new(),
        });
        let args = serde_json::json!({ "url": format!("{}/hello", server.url()) });
        let ctx = ToolContext::new("test", "msg");

        let error = tool
            .execute(args, &ctx)
            .await
            .err()
            .expect("127.0.0.1 must be refused");

        assert!(
            error.to_string().contains("private address"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn a_host_outside_the_allowlist_is_refused_without_resolving() {
        let tool = WebFetchTool::new(WebFetchPolicy {
            block_private_networks: false,
            allowed_hosts: vec!["ncbi.nlm.nih.gov".to_string()],
        });
        let ctx = ToolContext::new("test", "msg");

        let error = tool
            .execute(
                serde_json::json!({ "url": "https://example.org/page" }),
                &ctx,
            )
            .await
            .err()
            .expect("example.org is not allowed");

        assert!(error.to_string().contains("not in the allowed hosts"));
    }

    #[test]
    fn the_allowlist_is_normalized() {
        assert_eq!(
            WebFetchPolicy::parse_hosts(" *.PubMed.NCBI.nlm.nih.gov. , example.org ,, "),
            vec!["pubmed.ncbi.nlm.nih.gov", "example.org"]
        );
    }

    #[test]
    fn private_ranges_are_classified() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.5.5",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(
                !is_public_ip(ip.parse().expect("ip")),
                "{ip} must be treated as private"
            );
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(
                is_public_ip(ip.parse().expect("ip")),
                "{ip} must be treated as public"
            );
        }
    }
}
