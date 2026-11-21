//! Web Fetch Tool
//!
//! This tool allows the AI to retrieve content from specified URLs.
//! It supports converting HTML responses into Markdown for easier consumption by the LLM.
//!
//! # SSRF protections
//!
//! - Non-`http(s)` schemes are rejected (no `file:`, `gopher:`, ...).
//! - Private and link-local network addresses are blocked, including
//!   cloud-metadata endpoints like `169.254.169.254`. Loopback is allowed so
//!   the tool keeps working against local development servers.
//! - Redirects are validated hop-by-hop with the same rules, so a public
//!   URL cannot redirect into an internal address.

use crate::fs_utils::into_tool_error;
use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

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
#[derive(Debug, Clone)]
pub struct WebFetchPolicy {
    /// Refuse destinations that resolve to a private address. On by default.
    pub block_private_networks: bool,
    /// Host suffixes that may be fetched. Empty means "no host allowlist".
    pub allowed_hosts: Vec<String>,
}

impl Default for WebFetchPolicy {
    fn default() -> Self {
        Self {
            block_private_networks: true,
            allowed_hosts: Vec::new(),
        }
    }
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

/// Returns true if the host resolves to a loopback address or is a
/// loopback-style name. Loopback is intentionally allowed (local dev
/// servers), everything else internal is blocked by [`is_blocked_host`].
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    let host = host.to_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    matches!(
        host.parse::<IpAddr>(),
        Ok(IpAddr::V4(v4)) if v4.is_loopback()
    ) || matches!(host.parse::<IpAddr>(), Ok(IpAddr::V6(v6)) if v6.is_loopback())
}

/// Returns true if the host must not be fetched: private networks,
/// link-local (incl. cloud metadata endpoints), and unspecified addresses.
fn is_blocked_host(host: &str) -> bool {
    let host = host.trim().to_lowercase();
    if host == "metadata.google.internal" {
        return true;
    }
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    match literal.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            v4.is_private() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast()
        }
        Ok(IpAddr::V6(v6)) => {
            v6.is_unique_local() || v6.is_unicast_link_local() || v6.is_unspecified()
        }
        Err(_) => false,
    }
}

/// Validates a URL is fetchable: http(s) only, host present, and the host is
/// not an internal address.
fn validate_url(url: &Url, policy: &WebFetchPolicy) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(format!("unsupported URL scheme: {}", other)),
    }
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    if !policy.permits_host(host) {
        return Err(format!("host '{}' is not in the webfetch allowlist", host));
    }
    if policy.block_private_networks && is_blocked_host(host) {
        return Err(format!(
            "access to internal host '{}' is blocked (private address or link-local network)",
            host
        ));
    }
    Ok(())
}

/// Redirects are followed one hop at a time so each destination is
/// validated (scheme, host, DNS, private-address check) before it is
/// contacted. More hops than this is treated as a loop.
const MAX_REDIRECTS: usize = 5;

impl WebFetchTool {
    /// Validates one hop and, for a DNS name, resolves it so the connection
    /// can be pinned to the address that was checked. IP literals and
    /// loopback names are not pinned: there is no resolution to race, and
    /// pinning `localhost` would break dual-stack dev servers.
    async fn check(
        &self,
        url: &Url,
    ) -> Result<(String, Option<std::net::SocketAddr>), Box<dyn Error + Send + Sync>> {
        validate_url(url, &self.policy).map_err(|e| into_tool_error(anyhow::anyhow!(e)))?;
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("URL has no host"))?
            .to_string();
        let literal = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok();
        if literal || is_loopback_host(&host) {
            return Ok((host, None));
        }

        let port = url.port_or_known_default().unwrap_or(443);
        let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|e| anyhow::anyhow!("cannot resolve {host}: {e}"))?
            .collect();
        if addrs.is_empty() {
            return Err(anyhow::anyhow!("{host} did not resolve").into());
        }
        if self.policy.block_private_networks
            && let Some(private) = addrs
                .iter()
                .find(|addr| is_blocked_host(&addr.ip().to_string()))
        {
            return Err(anyhow::anyhow!(
                "access to internal host '{host}' is blocked: it resolves to {}, a private address",
                private.ip()
            )
            .into());
        }
        Ok((host, Some(addrs[0])))
    }

    /// Fetches `start`, following redirects and re-validating every hop.
    /// One cookie jar spans all hops, so a `Set-Cookie` on the redirect is
    /// sent to the destination (identity-provider round trips rely on it).
    async fn fetch_following(&self, start: Url) -> Result<String, Box<dyn Error + Send + Sync>> {
        let jar = Arc::new(reqwest::cookie::Jar::default());
        let start_display = start.to_string();
        let mut current = start;

        for hop in 0..=MAX_REDIRECTS {
            let (host, pinned) = self.check(&current).await?;
            let mut builder = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                // Honest attribution: identify as GeWu instead of posing as a
                // browser. The version follows the crate so it cannot go stale.
                .user_agent(concat!("gewu/", env!("CARGO_PKG_VERSION")))
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
                    return Err(anyhow::anyhow!(
                        "more than {MAX_REDIRECTS} redirects starting at {start_display}"
                    )
                    .into());
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| anyhow::anyhow!("redirect without a Location header"))?;
                current = current
                    .join(location)
                    .map_err(|e| anyhow::anyhow!("bad redirect target {location}: {e}"))?;
                continue;
            }

            if !status.is_success() {
                return Ok(format!("Request failed with status: {}", status));
            }

            return response.text().await.map_err(into_tool_error);
        }

        Err(anyhow::anyhow!("too many redirects starting at {start_display}").into())
    }
}

/// Case-insensitive byte search that keeps offsets valid for the original
/// string (no lowercased copy, which can shift byte indices for non-ASCII).
fn find_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || haystack.len() < needle.len() || from > haystack.len() - needle.len() {
        return None;
    }
    (from..=haystack.len() - needle.len())
        .find(|&i| haystack[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

/// Index of the first opening `<tag` that is a real tag (followed by
/// whitespace, `>` or `/`), so `<styled>` does not match `style`.
fn find_tag_open(html: &str, tag: &str, from: usize) -> Option<usize> {
    let open = format!("<{tag}");
    let mut from = from;
    while let Some(at) = find_ci(html, &open, from) {
        let next = html.as_bytes().get(at + open.len()).copied();
        let is_tag = matches!(next, Some(b' ' | b'>' | b'/' | b'\t' | b'\n' | b'\r'));
        if is_tag {
            return Some(at);
        }
        from = at + open.len();
    }
    None
}

/// Removes everything outside the first `<tag>…</tag>` element, counting
/// nesting of the same tag so inner elements stay inside the slice.
fn element_slice<'a>(html: &'a str, tag: &str) -> Option<&'a str> {
    let open_len = tag.len() + 1;
    let close = format!("</{tag}");
    let start = find_tag_open(html, tag, 0)?;
    let mut depth = 1usize;
    let mut pos = start + open_len;
    while pos < html.len() {
        let next_open = find_tag_open(html, tag, pos);
        let next_close = find_ci(html, &close, pos);
        match (next_open, next_close) {
            (None, None) => return None,
            (Some(open_at), Some(close_at)) if open_at <= close_at => {
                depth += 1;
                pos = open_at + open_len;
            }
            (_, Some(close_at)) => {
                let end = find_ci(html, ">", close_at).map(|i| i + 1)?;
                depth -= 1;
                if depth == 0 {
                    return html.get(start..end);
                }
                pos = end;
            }
            (Some(_), None) => return None,
        }
    }
    None
}

/// Strips HTML comments (`<!-- … -->`).
fn strip_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Removes one non-content element (`<script>`, `<style>`, …) with its body.
fn strip_element(html: &str, tag: &str) -> String {
    let open_len = tag.len() + 1;
    let close = format!("</{tag}");
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = find_tag_open(rest, tag, 0) {
        out.push_str(&rest[..start]);
        match find_ci(rest, &close, start + open_len) {
            Some(close_at) => {
                rest = match find_ci(rest, ">", close_at) {
                    Some(end) => &rest[end + 1..],
                    None => "",
                };
            }
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Extracts the content worth converting: non-content elements and comments
/// removed, narrowed to the document's `<article>` or `<main>` when it has
/// one. html2md keeps inline `<script>` bodies, so a script-heavy page
/// otherwise converts to mostly JavaScript and pushes the real content past
/// the tool-output limit.
fn content_html(html: &str) -> String {
    const NON_CONTENT: [&str; 6] = ["script", "style", "noscript", "svg", "template", "iframe"];
    let mut cleaned = html.to_string();
    for tag in NON_CONTENT {
        cleaned = strip_element(&cleaned, tag);
    }
    cleaned = strip_comments(&cleaned);
    for tag in ["article", "main"] {
        // Only trust the container when it holds a real article; a tiny
        // `<article>` is usually a teaser, and narrowing would drop the page.
        if let Some(slice) = element_slice(&cleaned, tag)
            && slice.len() > 500
        {
            return slice.to_string();
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

        let mut raw = input.url;
        if !raw.contains("://") {
            raw = format!("https://{}", raw);
        }
        let parsed =
            Url::parse(&raw).map_err(|e| into_tool_error(anyhow::anyhow!(e.to_string())))?;

        // Upgrade plain http to https for public hosts; keep http for
        // loopback so local development servers keep working.
        let loopback = parsed.host_str().map(is_loopback_host).unwrap_or(false);
        let url = if parsed.scheme() == "http" && !loopback {
            parsed.to_string().replacen("http://", "https://", 1)
        } else {
            parsed.to_string()
        };
        let final_url =
            Url::parse(&url).map_err(|e| into_tool_error(anyhow::anyhow!(e.to_string())))?;

        let html = self.fetch_following(final_url).await?;

        let output = match input.format.as_str() {
            "html" => html,
            _ => html2md::parse_html(&content_html(&html)),
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
            .match_header("user-agent", concat!("gewu/", env!("CARGO_PKG_VERSION")))
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body("<html><body><h1>Hello, World!</h1></body></html>")
            .create_async()
            .await;

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

    #[test]
    fn private_addresses_are_blocked() {
        assert!(is_blocked_host("10.0.0.5"));
        assert!(is_blocked_host("192.168.1.1"));
        assert!(is_blocked_host("172.16.0.9"));
        assert!(is_blocked_host("169.254.169.254"));
        assert!(is_blocked_host("0.0.0.0"));
        assert!(is_blocked_host("fd00::1"));
        assert!(is_blocked_host("fe80::1"));
        assert!(is_blocked_host("metadata.google.internal"));
    }

    #[test]
    fn public_and_loopback_are_allowed() {
        assert!(!is_blocked_host("93.184.216.34"));
        assert!(!is_blocked_host("example.com"));
        assert!(!is_blocked_host("127.0.0.1"));
        assert!(!is_blocked_host("::1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("[::1]"));
    }

    #[test]
    fn policy_normalizes_and_matches_hosts() {
        let hosts = WebFetchPolicy::parse_hosts("*.Example.COM, foo.,, bar");
        assert_eq!(hosts, vec!["example.com", "foo", "bar"]);
        let policy = WebFetchPolicy {
            block_private_networks: true,
            allowed_hosts: hosts,
        };
        assert!(policy.permits_host("example.com"));
        assert!(policy.permits_host("a.example.com"));
        assert!(!policy.permits_host("badexample.com"));
        assert!(WebFetchPolicy::default().permits_host("anything.test"));
    }

    #[test]
    fn private_literals_follow_the_policy() {
        let url = Url::parse("http://10.0.0.5/").unwrap();
        assert!(validate_url(&url, &WebFetchPolicy::default()).is_err());
        let permissive = WebFetchPolicy {
            block_private_networks: false,
            allowed_hosts: Vec::new(),
        };
        assert!(validate_url(&url, &permissive).is_ok());
    }

    #[test]
    fn non_http_schemes_are_rejected() {
        let policy = WebFetchPolicy::default();
        let url = Url::parse("file:///etc/passwd").unwrap();
        assert!(validate_url(&url, &policy).is_err());
        let url = Url::parse("gopher://example.com").unwrap();
        assert!(validate_url(&url, &policy).is_err());
    }

    #[test]
    fn script_and_style_bodies_are_removed() {
        let html = "<p>keep</p><script>var leak = 'js';</script><style>.x{color:red}</style>";
        let cleaned = content_html(html);
        assert!(cleaned.contains("keep"));
        assert!(!cleaned.contains("leak"));
        assert!(!cleaned.contains("color:red"));
    }

    #[test]
    fn comments_and_other_non_content_elements_are_removed() {
        let html = "<!-- nav --><div>keep</div><noscript>no</noscript><svg><path d=\"x\"/></svg>                    <template><p>tpl</p></template><iframe src=\"https://x\"></iframe>";
        let cleaned = content_html(html);
        assert!(cleaned.contains("keep"));
        for leak in ["nav", "no</noscript>", "<path", "tpl", "<iframe"] {
            assert!(!cleaned.contains(leak), "leaked {leak}: {cleaned}");
        }
    }

    #[test]
    fn article_or_main_narrows_the_slice() {
        let body = "story ".repeat(100);
        let html = format!(
            "<nav>menu</nav><main><article><p>{body}</p></article></main><footer>end</footer>"
        );
        let cleaned = content_html(&html);
        assert!(cleaned.contains("story"));
        assert!(!cleaned.contains("menu"));
        assert!(!cleaned.contains("end"));

        let html = format!("<header>top</header><main><p>{body}</p></main><footer>bottom</footer>");
        let cleaned = content_html(&html);
        assert!(cleaned.contains("story"));
        assert!(!cleaned.contains("top"));
        assert!(!cleaned.contains("bottom"));
    }

    #[test]
    fn tiny_article_containers_do_not_narrow_the_page() {
        let html = "<article><p>teaser</p></article><section><p>real body</p></section>";
        let cleaned = content_html(html);
        assert!(cleaned.contains("teaser"));
        assert!(cleaned.contains("real body"));
    }

    #[test]
    fn nested_article_tags_keep_inner_content() {
        let body = "inner ".repeat(100);
        let html =
            format!("<article>outer<article>{body}</article>tail</article><aside>drop</aside>");
        let cleaned = content_html(&html);
        assert!(cleaned.contains("inner"));
        assert!(cleaned.contains("tail"));
        assert!(!cleaned.contains("drop"));
    }

    #[test]
    fn tag_prefixes_do_not_match_longer_tag_names() {
        let html = "<styled-box>keep</styled-box><scripture>x</scripture>";
        let cleaned = content_html(html);
        assert!(cleaned.contains("keep"));
        assert!(cleaned.contains("x"));
    }

    #[test]
    fn unclosed_elements_do_not_eat_the_document() {
        let html = "<p>before</p><script>unclosed";
        let cleaned = content_html(html);
        assert!(cleaned.contains("before"));
        assert!(!cleaned.contains("unclosed"));
    }

    #[tokio::test]
    async fn scripts_are_stripped_before_markdown_conversion() -> Result<()> {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/page")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body(
                "<html><script>var junk = 'not content';</script>\
                 <body><h1>Title</h1><p>Real text.</p></body></html>",
            )
            .create_async()
            .await;

        let tool = WebFetchTool::default();
        let url = format!("{}/page", server.url());
        let args = serde_json::json!({ "url": url });
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        assert!(result.output.contains("Real text."));
        assert!(!result.output.contains("not content"));
        Ok(())
    }

    #[tokio::test]
    async fn redirects_follow_with_cookies() -> Result<()> {
        let mut server = mockito::Server::new_async().await;

        let _start = server
            .mock("GET", "/start")
            .with_status(302)
            .with_header("location", "/final")
            .with_header("set-cookie", "session=abc123; Path=/")
            .create_async()
            .await;
        let _final = server
            .mock("GET", "/final")
            .match_header("cookie", mockito::Matcher::Regex("session=abc123".into()))
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body("<p>after redirect</p>")
            .create_async()
            .await;

        let tool = WebFetchTool::default();
        let url = format!("{}/start", server.url());
        let args = serde_json::json!({ "url": url });
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        assert!(result.output.contains("after redirect"));
        _final.assert_async().await;
        Ok(())
    }

    #[tokio::test]
    async fn a_redirect_loop_is_bounded() -> Result<()> {
        let mut server = mockito::Server::new_async().await;
        let _loop = server
            .mock("GET", "/loop")
            .with_status(302)
            .with_header("location", "/loop")
            .create_async()
            .await;

        let tool = WebFetchTool::default();
        let url = format!("{}/loop", server.url());
        let args = serde_json::json!({ "url": url });
        let ctx = ToolContext::new("test", "msg");
        let err = tool
            .execute(args, &ctx)
            .await
            .expect_err("a redirect loop must stop");
        assert!(err.to_string().contains("redirect"), "{err}");
        Ok(())
    }

    #[tokio::test]
    async fn a_redirect_to_a_private_address_is_blocked_on_the_next_hop() -> Result<()> {
        let mut server = mockito::Server::new_async().await;
        let _hop = server
            .mock("GET", "/to-private")
            .with_status(302)
            .with_header("location", "http://10.0.0.5/internal")
            .create_async()
            .await;

        let tool = WebFetchTool::default();
        let url = format!("{}/to-private", server.url());
        let args = serde_json::json!({ "url": url });
        let ctx = ToolContext::new("test", "msg");
        let err = tool
            .execute(args, &ctx)
            .await
            .expect_err("the second hop must be validated");
        assert!(err.to_string().contains("blocked"), "{err}");
        Ok(())
    }

    #[tokio::test]
    async fn private_urls_fail_at_runtime() {
        let tool = WebFetchTool::default();
        let args = serde_json::json!({ "url": "http://169.254.169.254/latest/meta-data/" });
        let ctx = ToolContext::new("test", "msg");
        let err = tool
            .execute(args, &ctx)
            .await
            .expect_err("internal host must be rejected");
        assert!(err.to_string().contains("blocked"));
    }
}
