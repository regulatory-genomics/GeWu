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

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct RuntimeFrontendAssets {
    pub plugins: Vec<RuntimeFrontendPlugin>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeFrontendPlugin {
    pub id: String,
    pub sidecars: Vec<RuntimeFrontendSidecar>,
    pub styles: Vec<String>,
    pub scripts: Vec<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeFrontendSidecar {
    pub id: String,
    pub url: String,
}

impl RuntimeFrontendAssets {
    fn from_env() -> Self {
        std::env::var("GEWU_FRONTEND_PLUGINS")
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }
}

/// Resolve the static directory used for serving frontend assets.
///
/// Priority: `GEWU_STATIC_DIR` env → `<exe_dir>/gewu-web/static` → `gewu-web/static` (cwd).
pub fn resolve_static_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("GEWU_STATIC_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let candidate = exe_dir.join("gewu-web").join("static");
            if candidate.is_dir() {
                return candidate;
            }
        }
    }
    PathBuf::from("gewu-web/static")
}

fn index_path() -> PathBuf {
    resolve_static_dir().join("index.html")
}

pub async fn serve_index() -> Response {
    let path = index_path();
    match std::fs::read_to_string(&path) {
        Ok(body) => {
            let injected = inject_runtime_assets(&body, &RuntimeFrontendAssets::from_env());
            Html(injected).into_response()
        }
        Err(_) => {
            Html("<html><body><h1>GeWu</h1><p>index.html not found.</p></body></html>".to_string())
                .into_response()
        }
    }
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('"', "&quot;")
}

pub fn inject_runtime_assets(html: &str, assets: &RuntimeFrontendAssets) -> String {
    let runtime_json =
        serde_json::to_string(assets).unwrap_or_else(|_| "{\"plugins\":[]}".to_string());
    let runtime_script = format!("<script>window.__GEWU_PLUGINS__ = {runtime_json};</script>");
    let mut out = inject_before_head_close(html, &runtime_script);

    for plugin in &assets.plugins {
        for style in &plugin.styles {
            out = inject_before_head_close(
                &out,
                &format!(
                    "<link rel=\"stylesheet\" href=\"{}\" />",
                    escape_attr(style)
                ),
            );
        }
    }

    for plugin in &assets.plugins {
        for script in &plugin.scripts {
            out = inject_before_app_script(
                &out,
                &format!("<script src=\"{}\"></script>", escape_attr(script)),
            );
        }
    }

    out
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
    fn injects_generic_plugin_runtime_and_assets() {
        let html = "<html><head><title>x</title></head><body><script src=\"/canvas.js\"></script><script src=\"/app.js\"></script></body></html>";
        let assets = RuntimeFrontendAssets {
            plugins: vec![RuntimeFrontendPlugin {
                id: "tachyon".to_string(),
                sidecars: vec![RuntimeFrontendSidecar {
                    id: "browser".to_string(),
                    url: "http://127.0.0.1:8400".to_string(),
                }],
                styles: vec!["http://127.0.0.1:8400/assets/tachyon-canvas.css".to_string()],
                scripts: vec!["http://127.0.0.1:8400/assets/tachyon-canvas.iife.js".to_string()],
            }],
        };

        let out = super::inject_runtime_assets(html, &assets);

        assert!(out.contains("window.__GEWU_PLUGINS__"));
        assert!(out.contains("\"tachyon\""));
        assert!(out.contains("http://127.0.0.1:8400"));
        assert!(out.contains(
            "<link rel=\"stylesheet\" href=\"http://127.0.0.1:8400/assets/tachyon-canvas.css\" />"
        ));
        assert!(out.contains(
            "<script src=\"http://127.0.0.1:8400/assets/tachyon-canvas.iife.js\"></script>"
        ));
        let renderer_pos = out
            .find("src=\"http://127.0.0.1:8400/assets/tachyon-canvas.iife.js\"")
            .unwrap();
        let app_pos = out.find("src=\"/app.js\"").unwrap();
        assert!(renderer_pos < app_pos);
    }

    #[test]
    fn static_index_shell_does_not_hardcode_tachyon_renderer_assets() {
        let html = include_str!("../static/index.html");
        assert!(!html.contains("/vendor/tachyon-canvas/"));
        assert!(html.contains("/canvas.js"));
        assert!(html.contains("/app.js"));
    }

    #[test]
    fn canvas_controller_exposes_hydration_api() {
        let canvas_js = include_str!("../static/canvas.js");
        assert!(
            canvas_js.contains("function hydrate"),
            "canvas.js must contain 'function hydrate'"
        );
        assert!(
            canvas_js.contains("hydrate,"),
            "canvas.js must expose 'hydrate' in window.canvasController"
        );
        assert!(
            canvas_js.contains("hasRenderer"),
            "canvas.js must expose 'hasRenderer' in window.canvasController"
        );
    }

    #[test]
    fn app_fetches_canvas_state_for_session_hydration() {
        let app_js = include_str!("../static/app.js");
        assert!(
            app_js.contains("/api/sessions/${encodeURIComponent(sessionId)}/canvas"),
            "app.js must fetch canvas state from session canvas endpoint"
        );
        assert!(
            app_js.contains("window.canvasController.hydrate"),
            "app.js must call window.canvasController.hydrate for session hydration"
        );
    }

    #[test]
    fn resolve_static_dir_uses_env_var_when_set() {
        let dir = std::env::temp_dir().join("gewu-static-dir-test");
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("GEWU_STATIC_DIR").ok();
        unsafe { std::env::set_var("GEWU_STATIC_DIR", &dir) };

        let resolved = resolve_static_dir();

        unsafe {
            match prev {
                Some(v) => std::env::set_var("GEWU_STATIC_DIR", v),
                None => std::env::remove_var("GEWU_STATIC_DIR"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(resolved, dir);
    }

    #[test]
    fn resolve_static_dir_falls_back_to_cwd_relative_when_no_env_or_exe_sibling() {
        let prev_env = std::env::var("GEWU_STATIC_DIR").ok();
        unsafe { std::env::remove_var("GEWU_STATIC_DIR") };

        let resolved = resolve_static_dir();

        unsafe {
            if let Some(v) = prev_env {
                std::env::set_var("GEWU_STATIC_DIR", v);
            }
        }

        let exe_sibling = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("gewu-web").join("static")));
        let cwd_relative = PathBuf::from("gewu-web/static");

        let acceptable = match exe_sibling {
            Some(ref p) if p.is_dir() => resolved == *p,
            _ => resolved == cwd_relative,
        };
        assert!(
            acceptable,
            "resolve_static_dir returned {:?}; expected exe-sibling or cwd-relative fallback",
            resolved
        );
    }
}
