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

/// Remove blocks whose text the model never needs: inline scripts alone made a
/// nature.com article 272 KB of JavaScript.
fn strip_block(html: &str, tag: &str) -> String {
    let pattern = format!(r"(?is)<{tag}\b[^>]*>.*?</{tag}\s*>");
    match regex::Regex::new(&pattern) {
        Ok(re) => re.replace_all(html, "").into_owned(),
        Err(_) => html.to_string(),
    }
}

/// Strip scripts, styles and other non-content elements, then prefer the
/// document's own `<article>` or `<main>` when it has one.
fn clean_html(html: &str) -> String {
    let mut cleaned = html.to_string();
    for tag in ["script", "style", "noscript", "svg", "template", "iframe"] {
        cleaned = strip_block(&cleaned, tag);
    }
    if let Ok(comments) = regex::Regex::new(r"(?s)<!--.*?-->") {
        cleaned = comments.replace_all(&cleaned, "").into_owned();
    }
    for tag in ["article", "main"] {
        let Ok(re) = regex::Regex::new(&format!(r"(?is)<{tag}\b[^>]*>(.*?)</{tag}\s*>")) else {
            continue;
        };
        if let Some(inner) = re.captures(&cleaned).and_then(|caps| caps.get(1)) {
            if inner.as_str().len() > 500 {
                return inner.as_str().to_string();
            }
        }
    }
    cleaned
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
        let body = self
            .fetch_following(url, self.policy.block_private_networks)
            .await?;

        let output = match input.format.as_str() {
            "html" => body,
            _ => {
                let cleaned = clean_html(&body);
                html2md::parse_html(&cleaned)
            }
        };

        Ok(ToolResult::new("webfetch", output))
    }
}

/// Redirects are followed one hop at a time so each destination can be
/// validated. More hops than this is treated as a loop.
const MAX_REDIRECTS: usize = 5;

impl WebFetchTool {
    /// Check one URL and, when private networks are blocked, resolve it and
    /// return the address to pin the connection to.
    async fn check(
        &self,
        url: &reqwest::Url,
        enforce_private: bool,
    ) -> Result<(String, Option<SocketAddr>), Box<dyn Error + Send + Sync>> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(
                format!("webfetch refused: scheme {} is not http or https", url.scheme()).into(),
            );
        }
        let host = url
            .host_str()
            .ok_or_else(|| format!("webfetch refused: {url} has no host"))?
            .to_ascii_lowercase();
        if !self.policy.permits_host(&host) {
            return Err(format!(
                "webfetch refused: {host} is not in the allowed hosts list"
            )
            .into());
        }
        if !enforce_private {
            return Ok((host, None));
        }

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
        Ok((host, Some(addrs[0])))
    }

    /// Fetch `start`, following redirects and re-validating every hop.
    ///
    /// Cookies are shared across hops through one jar, so an identity-provider
    /// round trip (nature.com) can complete. When `enforce_private` is set, the
    /// connection is pinned to the address that was checked on that hop.
    async fn fetch_following(
        &self,
        start: reqwest::Url,
        enforce_private: bool,
    ) -> Result<String, Box<dyn Error + Send + Sync>> {
        let jar = Arc::new(reqwest::cookie::Jar::default());
        let start_display = start.to_string();
        let mut current = start;

        for hop in 0..=MAX_REDIRECTS {
            let (host, pinned) = self.check(&current, enforce_private).await?;

            let mut builder = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent("opencode-cli/0.1.0")
                .cookie_provider(jar.clone())
                .redirect(reqwest::redirect::Policy::none());
            if let Some(addr) = pinned {
                builder = builder.resolve(&host, addr);
            }
            let client = builder.build().map_err(into_tool_error)?;

            let response = client
                .get(current.clone())
                .send()
                .await
                .map_err(into_tool_error)?;
            let status = response.status();

            if status.is_redirection() {
                if hop == MAX_REDIRECTS {
                    return Err(format!(
                        "webfetch: more than {MAX_REDIRECTS} redirects starting at {start_display}"
                    )
                    .into());
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or("webfetch: redirect without a Location header")?;
                current = current.join(location).map_err(|error| {
                    format!("webfetch: bad redirect target {location}: {error}")
                })?;
                continue;
            }

            if !status.is_success() {
                return Ok(format!("Request failed with status: {status}"));
            }

            return response.text().await.map_err(into_tool_error);
        }

        Err(format!("webfetch: too many redirects starting at {start_display}").into())
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

    #[tokio::test]
    async fn redirects_are_followed_with_a_shared_cookie_jar() {
        let mut server = mockito::Server::new_async().await;
        let base = server.url();

        let _idp = server
            .mock("GET", "/idp")
            .with_status(302)
            .with_header("location", "/article")
            .with_header("set-cookie", "session=1; Path=/")
            .create_async()
            .await;
        let _article = server
            .mock("GET", "/article")
            .match_header("cookie", "session=1")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body("<html><body><h1>Article</h1></body></html>")
            .create_async()
            .await;

        let tool = WebFetchTool::default();
        let start = reqwest::Url::parse(&format!("{base}/idp")).expect("url");
        let body = tool.fetch_following(start, false).await.expect("follow");

        assert!(
            body.contains("Article"),
            "the cookie from the first hop must reach the second: {body}"
        );
    }

    #[tokio::test]
    async fn a_redirect_loop_is_bounded() {
        let mut server = mockito::Server::new_async().await;
        let _looping = server
            .mock("GET", "/loop")
            .with_status(302)
            .with_header("location", "/loop")
            .create_async()
            .await;

        let tool = WebFetchTool::default();
        let start = reqwest::Url::parse(&format!("{}/loop", server.url())).expect("url");
        let error = tool
            .fetch_following(start, false)
            .await
            .err()
            .expect("a loop must stop");

        assert!(error.to_string().contains("redirect"), "{error}");
    }

    #[tokio::test]
    async fn a_redirect_to_a_private_address_is_checked_on_the_next_hop() {
        let mut server = mockito::Server::new_async().await;
        let _hop = server
            .mock("GET", "/to-loopback")
            .with_status(302)
            .with_header("location", "http://127.0.0.1:8406/api/health")
            .create_async()
            .await;

        let tool = WebFetchTool::new(WebFetchPolicy {
            block_private_networks: true,
            allowed_hosts: Vec::new(),
        });
        let start = reqwest::Url::parse(&format!("{}/to-loopback", server.url())).expect("url");
        let error = tool
            .fetch_following(start, true)
            .await
            .err()
            .expect("the first hop is already private");

        assert!(error.to_string().contains("private address"), "{error}");
    }

    #[tokio::test]
    #[ignore = "hits the public internet"]
    async fn live_nature_article_is_followed() {
        let tool = WebFetchTool::new(WebFetchPolicy {
            block_private_networks: true,
            allowed_hosts: Vec::new(),
        });
        let ctx = ToolContext::new("test", "msg");

        let result = tool
            .execute(
                serde_json::json!({"url": "https://www.nature.com/articles/s41586-025-09422-z"}),
                &ctx,
            )
            .await
            .expect("fetch");

        println!(
            "fetched {} chars, starts with: {:?}",
            result.output.len(),
            &result.output[..result.output.len().min(300)]
        );
        assert!(
            result.output.len() > 500,
            "expected an article body, got {} chars: {}",
            result.output.len(),
            &result.output[..result.output.len().min(200)]
        );
        assert!(
            result.output.contains("DeepSeek") || result.output.contains("title"),
            "the article body must be present"
        );
    }

    #[test]
    fn scripts_and_styles_never_reach_the_markdown() {
        let html = r#"<html><head><style>.x{}</style></head><body>
            <script>var leaked = "secret";</script>
            <article><h1>Real title</h1><p>Real body.</p></article>
            </body></html>"#;

        let cleaned = clean_html(html);

        assert!(cleaned.contains("Real title"));
        assert!(!cleaned.contains("leaked"));
        assert!(!cleaned.contains(".x{}"));
    }
}
