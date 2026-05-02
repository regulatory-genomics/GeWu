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
            let injected = inject_runtime_meta(&body);
            Html(injected).into_response()
        }
        Err(_) => Html(
            "<html><body><h1>GeWu</h1><p>index.html not found.</p></body></html>".to_string(),
        )
        .into_response(),
    }
}

fn inject_runtime_meta(html: &str) -> String {
    let url = std::env::var("TACHYON_BROWSER_URL").unwrap_or_default();
    let url_attr = url
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('"', "&quot;");
    let meta = format!(
        "<meta name=\"tachyon-browser-url\" content=\"{}\" />",
        url_attr
    );
    if let Some(idx) = html.find("</head>") {
        let mut out = String::with_capacity(html.len() + meta.len());
        out.push_str(&html[..idx]);
        out.push_str(&meta);
        out.push_str(&html[idx..]);
        out
    } else {
        // No <head>; prepend so the meta lands before <body> at minimum.
        format!("{meta}\n{html}")
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
        let out = super::inject_runtime_meta(html);
        assert!(out.contains("name=\"tachyon-browser-url\""));
        let head_close = out.find("</head>").unwrap();
        let meta_pos = out.find("name=\"tachyon-browser-url\"").unwrap();
        assert!(meta_pos < head_close);
    }
}
