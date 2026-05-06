//! Index-serving helper.
//!
//! Static asset serving is handled by `tower_http::services::ServeDir`
//! (configured in `lib.rs`); ServeDir does proper path containment checks
//! so we don't need a hand-rolled traversal-prone fallback any more.
//!
//! `serve_index` is still used as the explicit handler for `/` and
//! `/index.html` because we want those to load the SPA shell even if
//! ServeDir's directory-index handling differs across versions.

use axum::response::{Html, IntoResponse, Response};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct RuntimeFrontendAssets {
    pub tachyon_browser_url: Option<String>,
    pub renderer_style: Option<String>,
    pub renderer_script: Option<String>,
}

impl RuntimeFrontendAssets {
    fn from_env() -> Self {
        Self {
            tachyon_browser_url: non_empty_env("TACHYON_BROWSER_URL"),
            renderer_style: non_empty_env("GEWU_TACHYON_RENDERER_STYLE"),
            renderer_script: non_empty_env("GEWU_TACHYON_RENDERER_SCRIPT"),
        }
    }
}

/// Resolved path to gewu-web/static/index.html.
fn index_path() -> PathBuf {
    Path::new("gewu-web").join("static").join("index.html")
}

pub async fn serve_index() -> Response {
    let path = index_path();
    match std::fs::read_to_string(&path) {
        Ok(body) => {
            // Inject the tachyon-browser URL (if configured at startup) as a
            // <meta> tag the canvas IIFE picks up. This lets the canvas reach
            // a non-same-origin tachyon-browser without hard-coding a URL in
            // the static HTML. When TACHYON_BROWSER_URL is unset, the meta
            // resolves to "" and the canvas falls back to same-origin.
            let injected = inject_runtime_assets(&body, &RuntimeFrontendAssets::from_env());
            Html(injected).into_response()
        }
        Err(_) => {
            Html("<html><body><h1>GeWu</h1><p>index.html not found.</p></body></html>".to_string())
                .into_response()
        }
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('"', "&quot;")
}

pub fn inject_runtime_assets(html: &str, assets: &RuntimeFrontendAssets) -> String {
    let with_meta = inject_runtime_meta(html, assets.tachyon_browser_url.as_deref().unwrap_or(""));
    let with_style = match assets.renderer_style.as_deref() {
        Some(style) => inject_before_head_close(
            &with_meta,
            &format!(
                "<link rel=\"stylesheet\" href=\"{}\" />",
                escape_attr(style)
            ),
        ),
        None => with_meta,
    };
    match assets.renderer_script.as_deref() {
        Some(script) => inject_before_app_script(
            &with_style,
            &format!("<script src=\"{}\"></script>", escape_attr(script)),
        ),
        None => with_style,
    }
}

fn inject_runtime_meta(html: &str, url: &str) -> String {
    let url_attr = escape_attr(url);
    let meta = format!(
        "<meta name=\"tachyon-browser-url\" content=\"{}\" />",
        url_attr
    );
    inject_before_head_close(html, &meta)
}

fn inject_before_head_close(html: &str, snippet: &str) -> String {
    if let Some(idx) = html.find("</head>") {
        let mut out = String::with_capacity(html.len() + snippet.len());
        out.push_str(&html[..idx]);
        out.push_str(snippet);
        out.push_str(&html[idx..]);
        out
    } else {
        // No <head>; prepend so the meta lands before <body> at minimum.
        format!("{snippet}\n{html}")
    }
}

fn inject_before_app_script(html: &str, snippet: &str) -> String {
    let needle = "<script src=\"/app.js\"></script>";
    if let Some(idx) = html.find(needle) {
        let mut out = String::with_capacity(html.len() + snippet.len());
        out.push_str(&html[..idx]);
        out.push_str(snippet);
        out.push_str(&html[idx..]);
        out
    } else if let Some(idx) = html.find("</body>") {
        let mut out = String::with_capacity(html.len() + snippet.len());
        out.push_str(&html[..idx]);
        out.push_str(snippet);
        out.push_str(&html[idx..]);
        out
    } else {
        format!("{html}\n{snippet}")
    }
}

/// Verify a relative path stays within `base` after normalisation.
///
/// Used by tests; ServeDir does the same thing internally for the live
/// fallback path. We keep this here as a guard rail for any code that
/// later builds paths from user input.
pub fn safe_join(base: &Path, candidate: &str) -> Option<PathBuf> {
    let candidate_path = Path::new(candidate);
    if candidate_path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::from(base);
    for component in candidate_path.components() {
        match component {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            // Reject .. and any prefix/root components.
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let canonical_base = std::fs::canonicalize(base).ok()?;
    let canonical_out = std::fs::canonicalize(&out).ok()?;
    if canonical_out.starts_with(&canonical_base) {
        Some(canonical_out)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_rejects_dotdot() {
        let base = std::env::temp_dir();
        assert!(safe_join(&base, "../etc/passwd").is_none());
        assert!(safe_join(&base, "..").is_none());
        assert!(safe_join(&base, "/etc/passwd").is_none());
    }

    #[test]
    fn inject_runtime_meta_inserts_before_head_close() {
        let html = "<html><head><title>x</title></head><body></body></html>";
        let out = super::inject_runtime_meta(html, "");
        assert!(out.contains("name=\"tachyon-browser-url\""));
        let head_close = out.find("</head>").unwrap();
        let meta_pos = out.find("name=\"tachyon-browser-url\"").unwrap();
        assert!(meta_pos < head_close);
    }

    #[test]
    fn inject_runtime_assets_adds_renderer_style_and_script() {
        let html = "<html><head><title>x</title></head><body><script src=\"/canvas.js\"></script><script src=\"/app.js\"></script></body></html>";
        let assets = RuntimeFrontendAssets {
            tachyon_browser_url: Some("http://127.0.0.1:8400".to_string()),
            renderer_style: Some("/plugins/tachyon/tachyon-canvas.css".to_string()),
            renderer_script: Some("/plugins/tachyon/tachyon-canvas.iife.js".to_string()),
        };

        let out = super::inject_runtime_assets(html, &assets);

        assert!(out.contains("name=\"tachyon-browser-url\" content=\"http://127.0.0.1:8400\""));
        assert!(
            out.contains(
                "<link rel=\"stylesheet\" href=\"/plugins/tachyon/tachyon-canvas.css\" />"
            )
        );
        assert!(out.contains("<script src=\"/plugins/tachyon/tachyon-canvas.iife.js\"></script>"));
        let canvas_pos = out.find("src=\"/canvas.js\"").unwrap();
        let renderer_pos = out
            .find("src=\"/plugins/tachyon/tachyon-canvas.iife.js\"")
            .unwrap();
        let app_pos = out.find("src=\"/app.js\"").unwrap();
        assert!(canvas_pos < renderer_pos);
        assert!(renderer_pos < app_pos);
    }

    #[test]
    fn static_index_shell_does_not_hardcode_tachyon_renderer_assets() {
        let html = include_str!("../static/index.html");
        assert!(!html.contains("/vendor/tachyon-canvas/"));
        assert!(html.contains("/canvas.js"));
        assert!(html.contains("/app.js"));
    }
}
