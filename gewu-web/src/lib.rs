use anyhow::{Context, Result, bail};
use axum::{
    Router,
    http::HeaderValue,
    middleware,
    routing::{delete, get, post},
};
use clap::Parser;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

pub mod auth;
pub mod auth_middleware;
pub mod fallback;
pub mod plugin_suite;
pub mod routes;
pub mod sidecar;
pub mod state;
pub mod workspace;

use auth::AuthMode;
use fallback::{RuntimeFrontendAssets, RuntimeFrontendPlugin, RuntimeFrontendSidecar};
use gewu_cli::mcp_loader;
use gewu_runtime::PluginRegistry;
use plugin_suite::{FrontendAssetKind, PluginSuiteConfig};
use state::{AppState, PluginRegistrar};

#[derive(Parser, Debug)]
#[command(name = "gewu-web")]
#[command(version = "0.1.0")]
#[command(about = "GeWu Web UI - Browser-based interface for the GeWu agent framework")]
struct Cli {
    #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
    host: IpAddr,

    #[arg(long, default_value = "8080")]
    port: u16,

    #[arg(long)]
    config: Option<String>,

    #[arg(long)]
    api_key: Option<String>,

    #[arg(long)]
    endpoint: Option<String>,

    #[arg(long)]
    model: Option<String>,

    #[arg(long = "max-output-tokens", env = "GEWU_MAX_OUTPUT_TOKENS")]
    max_output_tokens: Option<u32>,

    #[arg(long)]
    db_path: Option<String>,

    /// Enable cookie-based password auth. When omitted, the server runs with
    /// no auth (open mode). Required when binding to a non-loopback host
    /// unless `--allow-insecure-bind` is also set.
    #[arg(long, env = "GEWU_PASSWORD")]
    password: Option<String>,

    /// Permit non-loopback bind without a password (use only on trusted
    /// networks). Off by default.
    #[arg(long, default_value_t = false)]
    allow_insecure_bind: bool,

    /// Directory containing the gewu-web SPA static assets (index.html,
    /// app.js, vendor/...). Falls back to `GEWU_STATIC_DIR` env, then
    /// `<exe_dir>/gewu-web/static`, then cwd-relative `gewu-web/static`.
    #[arg(long = "static-dir", env = "GEWU_STATIC_DIR")]
    static_dir: Option<String>,
}

pub async fn run_with_plugins<F>(register_plugins: F) -> Result<()>
where
    F: Fn(&mut PluginRegistry) + Send + Sync + 'static,
{
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    if let Some(dir) = cli.static_dir.as_deref() {
        unsafe { std::env::set_var("GEWU_STATIC_DIR", dir) };
    }
    if std::env::var("GEWU_TACHYON_CANVAS_URL").is_err() {
        let host = if cli.host.is_unspecified() {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            cli.host
        };
        unsafe {
            std::env::set_var(
                "GEWU_TACHYON_CANVAS_URL",
                format!("http://{}:{}/api/workspaces/default", host, cli.port),
            );
        }
    }

    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    let loaded_config = gewu_cli::config::LoadedConfig::load(cli.config.as_deref())?;

    let api_key = cli
        .api_key
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .or(loaded_config.config.api_key.clone())
        .context("OPENAI_API_KEY not provided")?;

    let endpoint = cli
        .endpoint
        .or_else(|| std::env::var("OPENAI_ENDPOINT").ok())
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
        .or(loaded_config.config.base_url.clone())
        .context("Endpoint not provided")?;

    let model = cli
        .model
        .or_else(|| std::env::var("OPENAI_MODEL").ok())
        .or(loaded_config.config.model.clone())
        .unwrap_or_else(|| state::DEFAULT_MODEL.to_string());

    let max_output_tokens = cli
        .max_output_tokens
        .or_else(|| {
            std::env::var("GEWU_MAX_OUTPUT_TOKENS")
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
        })
        .or(loaded_config.config.max_output_tokens);

    let db_path = cli.db_path.or_else(|| {
        dirs::home_dir().map(|h| {
            h.join(".gewu")
                .join("gewu.db")
                .to_string_lossy()
                .to_string()
        })
    });

    let mcp_servers = loaded_config.config.mcp_servers;

    let tachyon_browser_cfg =
        parse_tachyon_browser_config(loaded_config.config.tachyon_browser.clone())?;
    let tachyon_suite_cfg =
        parse_tachyon_plugin_suite_config(loaded_config.config.tachyon_plugin_suite.clone())?;
    let plugin_suites = plugin_suite::normalize_plugin_suites(
        loaded_config.config.plugin_suites.clone(),
        loaded_config.config.tachyon_browser.clone(),
        loaded_config.config.tachyon_plugin_suite.clone(),
    )?;
    let effective_tachyon_browser_cfg = tachyon_browser_from_plugin_suites(&plugin_suites)?
        .unwrap_or_else(|| tachyon_suite_cfg.effective_sidecar_config(&tachyon_browser_cfg));
    let mut runtime_plugin_dirs = loaded_config.config.runtime_plugin_dirs;
    append_plugin_manifest_dirs(&mut runtime_plugin_dirs, &plugin_suites);

    let env_tachyon_url = std::env::var("TACHYON_BROWSER_URL").ok();
    let mut config = state::ServerConfig {
        api_key: Some(api_key),
        endpoint: Some(endpoint),
        model: Some(model),
        max_output_tokens,
        context_limit: None,
        temperature: None,
        db_path,
        plugins: loaded_config.config.plugins,
        plugin_sidecars: HashMap::new(),
        runtime_plugin_dirs,
        skill_dirs: loaded_config.config.skill_dirs,
        tachyon_browser_url: env_tachyon_url.clone(),
        tachyon_browser: effective_tachyon_browser_cfg,
    };

    let mcp_tools = mcp_loader::load_mcp_tools(mcp_servers).await?;

    let managed_tachyon_browser =
        sidecar::ensure_tachyon_browser(&config.tachyon_browser, env_tachyon_url.as_deref())
            .await?;
    if let Some(sidecar) = managed_tachyon_browser.as_ref() {
        config.tachyon_browser_url = Some(sidecar.url().to_string());
    }
    let runtime_assets =
        runtime_frontend_assets(&plugin_suites, config.tachyon_browser_url.as_deref())?;
    config.plugin_sidecars = plugin_sidecar_map(&runtime_assets);
    let browser_runtime_assets = browser_runtime_frontend_assets(&runtime_assets);
    // SAFETY: environment is configured during single-threaded startup before the router serves
    // index.html or static assets.
    unsafe {
        if let Some(url) = config.tachyon_browser_url.as_deref() {
            std::env::set_var("TACHYON_BROWSER_URL", url);
        }
        std::env::set_var(
            "GEWU_FRONTEND_PLUGINS",
            serde_json::to_string(&browser_runtime_assets)
                .unwrap_or_else(|_| "{\"plugins\":[]}".to_string()),
        );
    }

    let auth_mode = AuthMode::from_cli(cli.password.as_deref())?;
    if !cli.host.is_loopback() && !auth_mode.requires_password() && !cli.allow_insecure_bind {
        bail!(
            "non-loopback bind ({}) requires --password or --allow-insecure-bind",
            cli.host
        );
    }
    let content_security_policy = build_content_security_policy(&browser_runtime_assets);
    let plugin_registrar: PluginRegistrar = Arc::new(register_plugins);
    let app_state =
        Arc::new(AppState::new(config, auth_mode.clone(), mcp_tools, plugin_registrar).await?);

    // CORS policy:
    //
    // Default: deny cross-origin requests entirely (browsers fall back to
    // same-origin and never see CORS headers, which is what we want for
    // the local-dev server). Explicit allowlist via env var
    // GEWU_CORS_ALLOW_ORIGIN (comma-separated origins, "*" rejected).
    //
    // Older versions of this server applied `allow_origin: Any +
    // allow_methods: Any + allow_headers: Any`, which let any web page on
    // the user's machine reach the local agent. Disallowed.
    let cors = build_cors_layer();

    // ── Public routes (NO auth required) ──────────────────────────────────
    //
    // /api/validate intentionally does its own validation in-handler so
    // unauthenticated callers can probe whether a cookie is valid without
    // authenticating first. The login page and /api/login/logout are also
    // public so the user can authenticate.
    let public = Router::new()
        .route("/", get(fallback::serve_index))
        .route("/index.html", get(fallback::serve_index))
        .route("/login", get(routes::login::login_page))
        .route("/api/login", post(routes::login::handle_login))
        .route("/api/logout", post(routes::login::handle_logout))
        .route("/api/auth/mode", get(routes::login::handle_auth_mode))
        .route("/api/validate", get(routes::sessions::handle_validate));

    // ── Protected routes (token required) ─────────────────────────────────
    let protected = Router::new()
        .route("/api/info", get(routes::sessions::handle_server_info))
        .route("/api/agents", get(routes::sessions::handle_list_agents))
        .route("/api/skills", get(routes::skills::handle_list_skills))
        .route("/api/chat", post(routes::chat::handle_chat))
        .route(
            "/api/permissions/decide",
            post(routes::permissions::handle_permission_decision),
        )
        .route("/api/events", get(routes::events::handle_events))
        .route("/api/projects", get(routes::projects::handle_list_projects))
        .route(
            "/api/projects",
            post(routes::projects::handle_create_project),
        )
        .route(
            "/api/projects/tree",
            get(routes::projects::handle_projects_tree),
        )
        .route(
            "/api/projects/directories",
            get(routes::projects::handle_browse_directories),
        )
        .route("/api/workspaces", get(routes::workspaces::list_workspaces))
        .route(
            "/api/workspaces",
            post(routes::workspaces::create_workspace),
        )
        .route(
            "/api/workspaces/{workspace_id}/canvas/graph",
            get(routes::workspaces::get_graph).delete(routes::workspaces::delete_graph),
        )
        .route(
            "/api/workspaces/{workspace_id}/canvas/events",
            get(routes::workspaces::canvas_events),
        )
        .route(
            "/api/workspaces/{workspace_id}/workflow/events",
            get(routes::workspaces::get_workflow_events)
                .post(routes::workspaces::append_workflow_event),
        )
        .route(
            "/api/workspaces/{workspace_id}/workflow/lineage/{artifact_id}",
            get(routes::workspaces::get_lineage),
        )
        .route(
            "/api/workspaces/{workspace_id}/chat/send",
            post(routes::workspaces::workspace_chat_send),
        )
        .route("/api/runs", get(routes::runs::handle_list_runs))
        .route("/api/runs/{id}", get(routes::runs::handle_get_run))
        .route(
            "/api/runs/{id}/children",
            get(routes::runs::handle_get_run_children),
        )
        .route(
            "/api/runs/{id}/events",
            get(routes::runs::handle_get_run_events),
        )
        .route("/api/sessions", get(routes::sessions::handle_list_sessions))
        .route(
            "/api/sessions/tree",
            get(routes::sessions::handle_sessions_tree),
        )
        .route(
            "/api/sessions",
            post(routes::sessions::handle_create_session),
        )
        .route(
            "/api/sessions/{id}",
            delete(routes::sessions::handle_delete_session),
        )
        .route(
            "/api/sessions/{id}/messages",
            get(routes::sessions::handle_get_messages),
        )
        .route(
            "/api/sessions/{id}/work-graph",
            get(routes::work_graph::handle_get_work_graph),
        )
        .route(
            "/api/sessions/{id}/switch-agent",
            post(routes::sessions::handle_switch_agent),
        )
        .route(
            "/api/sessions/{id}/cancel",
            post(routes::sessions::handle_cancel_session),
        )
        .route(
            "/api/sessions/{id}/canvas",
            get(routes::canvas::handle_list),
        )
        .route(
            "/api/sessions/{id}/canvas/{canvas_id}",
            get(routes::canvas::handle_get).delete(routes::canvas::handle_close),
        )
        .route(
            "/api/sessions/{id}/canvas/{canvas_id}/state",
            post(routes::canvas::handle_patch),
        )
        .route(
            "/api/plugins/{plugin_id}/{sidecar_id}/{*path}",
            get(routes::plugin_proxy::handle_plugin_get)
                .post(routes::plugin_proxy::handle_plugin_post),
        )
        .route(
            "/api/browser/{*path}",
            get(routes::canvas::handle_browser_get).post(routes::canvas::handle_browser_post),
        )
        .route_layer(middleware::from_fn_with_state(
            app_state.clone(),
            auth_middleware::require_auth,
        ));

    let static_dir = fallback::resolve_static_dir();
    let static_service = ServeDir::new(&static_dir)
        .append_index_html_on_directories(false)
        .precompressed_gzip();

    let app = public
        .merge(protected)
        .fallback_service(static_service)
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::header::CONTENT_SECURITY_POLICY,
            content_security_policy,
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::header::X_FRAME_OPTIONS,
            axum::http::HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::header::X_CONTENT_TYPE_OPTIONS,
            axum::http::HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::header::REFERRER_POLICY,
            axum::http::HeaderValue::from_static("no-referrer"),
        ))
        .layer(cors)
        .with_state(app_state);

    let addr = SocketAddr::new(cli.host, cli.port);

    println!("========================================");
    println!("GeWu Web UI");
    println!("========================================");
    if auth_mode.requires_password() {
        println!("URL:  http://{}:{}/login", cli.host, cli.port);
        println!("Auth: cookie (password mode)");
    } else {
        println!("URL:  http://{}:{}/", cli.host, cli.port);
        println!("Auth: open (no password)");
    }
    println!("========================================");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    drop(managed_tachyon_browser);

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            let _ = signal.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

fn tachyon_browser_from_plugin_suites(
    suites: &[PluginSuiteConfig],
) -> Result<Option<sidecar::TachyonBrowserConfig>> {
    let Some(suite) = suites
        .iter()
        .find(|suite| suite.enabled && suite.id == "tachyon")
    else {
        return Ok(None);
    };
    let Some(browser) = suite.browser_sidecar_config()? else {
        return Ok(None);
    };
    Ok(Some(sidecar::TachyonBrowserConfig {
        enabled: browser.enabled,
        auto_start: browser.auto_start,
        url: browser.url,
        command: browser.command,
        args: browser.args,
        health_path: browser.health_path,
        startup_timeout_ms: browser.startup_timeout_ms,
        working_directory: browser.working_directory,
    }))
}

fn parse_tachyon_browser_config(value: serde_json::Value) -> Result<sidecar::TachyonBrowserConfig> {
    if value.is_null() {
        return Ok(sidecar::TachyonBrowserConfig::default());
    }
    serde_json::from_value(value).context("Failed to parse tachyonBrowser config")
}

fn parse_tachyon_plugin_suite_config(
    value: serde_json::Value,
) -> Result<sidecar::TachyonPluginSuiteConfig> {
    if value.is_null() {
        return Ok(sidecar::TachyonPluginSuiteConfig::default());
    }
    serde_json::from_value(value).context("Failed to parse tachyonPluginSuite config")
}

fn runtime_frontend_assets(
    suites: &[PluginSuiteConfig],
    tachyon_browser_url: Option<&str>,
) -> Result<RuntimeFrontendAssets> {
    let mut plugins = Vec::new();
    for suite in suites.iter().filter(|suite| suite.enabled) {
        let frontend = suite.runtime_frontend()?;
        let sidecars = frontend
            .sidecars
            .into_iter()
            .map(|sidecar| RuntimeFrontendSidecar {
                url: if suite.id == "tachyon" && sidecar.id == "browser" {
                    tachyon_browser_url.unwrap_or(&sidecar.url).to_string()
                } else {
                    sidecar.url
                },
                id: sidecar.id,
            })
            .collect();
        let mut styles = Vec::new();
        let mut scripts = Vec::new();
        for asset in frontend.assets {
            match asset.kind {
                FrontendAssetKind::Style => styles.push(asset.url),
                FrontendAssetKind::Script => scripts.push(asset.url),
            }
        }
        plugins.push(RuntimeFrontendPlugin {
            id: frontend.plugin_id,
            sidecars,
            styles,
            scripts,
        });
    }
    Ok(RuntimeFrontendAssets { plugins })
}

fn plugin_sidecar_map(assets: &RuntimeFrontendAssets) -> HashMap<String, HashMap<String, String>> {
    assets
        .plugins
        .iter()
        .map(|plugin| {
            let sidecars = plugin
                .sidecars
                .iter()
                .map(|sidecar| (sidecar.id.clone(), sidecar.url.clone()))
                .collect();
            (plugin.id.clone(), sidecars)
        })
        .collect()
}

fn browser_runtime_frontend_assets(assets: &RuntimeFrontendAssets) -> RuntimeFrontendAssets {
    let plugins = assets
        .plugins
        .iter()
        .map(|plugin| {
            let sidecars: Vec<RuntimeFrontendSidecar> = plugin
                .sidecars
                .iter()
                .map(|sidecar| RuntimeFrontendSidecar {
                    id: sidecar.id.clone(),
                    url: plugin_proxy_base(&plugin.id, &sidecar.id),
                })
                .collect();

            let styles = plugin
                .styles
                .iter()
                .map(|url| rewrite_plugin_asset_url(plugin, url))
                .collect();
            let scripts = plugin
                .scripts
                .iter()
                .map(|url| rewrite_plugin_asset_url(plugin, url))
                .collect();

            RuntimeFrontendPlugin {
                id: plugin.id.clone(),
                sidecars,
                styles,
                scripts,
            }
        })
        .collect();

    RuntimeFrontendAssets { plugins }
}

fn rewrite_plugin_asset_url(plugin: &RuntimeFrontendPlugin, url: &str) -> String {
    for sidecar in &plugin.sidecars {
        if let Some(proxy_url) = rewrite_url_for_sidecar(&plugin.id, sidecar, url) {
            return proxy_url;
        }
    }
    url.to_string()
}

fn rewrite_url_for_sidecar(
    plugin_id: &str,
    sidecar: &RuntimeFrontendSidecar,
    url: &str,
) -> Option<String> {
    let sidecar_url = sidecar.url.trim_end_matches('/');
    let suffix = url.strip_prefix(sidecar_url)?;
    if !suffix.is_empty() && !suffix.starts_with('/') && !suffix.starts_with('?') {
        return None;
    }
    Some(format!(
        "{}{}",
        plugin_proxy_base(plugin_id, &sidecar.id),
        suffix
    ))
}

fn plugin_proxy_base(plugin_id: &str, sidecar_id: &str) -> String {
    format!("/api/plugins/{plugin_id}/{sidecar_id}")
}

fn append_plugin_manifest_dirs(dirs: &mut Vec<String>, suites: &[PluginSuiteConfig]) {
    for suite in suites.iter().filter(|suite| suite.enabled) {
        let Some(manifest) = suite
            .manifest
            .as_deref()
            .filter(|path| !path.trim().is_empty())
        else {
            continue;
        };
        let dir = Path::new(manifest)
            .parent()
            .unwrap_or_else(|| Path::new(manifest))
            .to_string_lossy()
            .to_string();
        if !dirs.iter().any(|existing| existing == &dir) {
            dirs.push(dir);
        }
    }
}

fn build_content_security_policy(assets: &RuntimeFrontendAssets) -> HeaderValue {
    let mut origins = plugin_csp_origins(assets);
    origins.sort();
    origins.dedup();

    let mut connect_src = "connect-src 'self'".to_string();
    let mut script_src = "script-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net".to_string();
    let mut style_src = "style-src 'self' 'unsafe-inline'".to_string();
    for origin in origins {
        connect_src.push(' ');
        connect_src.push_str(&origin);
        script_src.push(' ');
        script_src.push_str(&origin);
        style_src.push(' ');
        style_src.push_str(&origin);
    }
    let csp = format!(
        "default-src 'self'; img-src 'self' data:; \
         {script_src}; {style_src}; \
         {connect_src}; font-src 'self' data:"
    );
    HeaderValue::from_str(&csp).unwrap_or_else(|_| HeaderValue::from_static("default-src 'self'"))
}

fn plugin_csp_origins(assets: &RuntimeFrontendAssets) -> Vec<String> {
    let mut origins = Vec::new();
    for plugin in &assets.plugins {
        for sidecar in &plugin.sidecars {
            if let Some(origin) = csp_origin(&sidecar.url) {
                origins.push(origin);
            }
        }
        for style in &plugin.styles {
            if let Some(origin) = csp_origin(style) {
                origins.push(origin);
            }
        }
        for script in &plugin.scripts {
            if let Some(origin) = csp_origin(script) {
                origins.push(origin);
            }
        }
    }
    origins
}

fn csp_origin(url: &str) -> Option<String> {
    let raw = url.trim();
    if raw.is_empty() {
        return None;
    }
    let parsed = reqwest::Url::parse(raw).ok()?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return None,
    }
    let host = parsed.host_str()?;
    let mut origin = format!("{}://{}", parsed.scheme(), host);
    if let Some(port) = parsed.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    Some(origin)
}

/// Build the CORS layer. By default disallows cross-origin requests so the
/// local-dev server is not silently reachable by other web pages on the
/// user's machine. Honors `GEWU_CORS_ALLOW_ORIGIN` (comma-separated origins)
/// when an explicit allowlist is needed.
fn build_cors_layer() -> CorsLayer {
    use axum::http::{HeaderValue, Method, header};

    let methods = [Method::GET, Method::POST, Method::DELETE, Method::OPTIONS];
    let headers = [header::CONTENT_TYPE, header::ACCEPT];
    let base = CorsLayer::new()
        .allow_methods(methods)
        .allow_headers(headers);

    let raw = std::env::var("GEWU_CORS_ALLOW_ORIGIN").unwrap_or_default();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        // Strict default: don't emit Access-Control-Allow-Origin at all.
        // Browsers will deny cross-origin XHR/fetch; same-origin still works.
        return base;
    }
    if trimmed == "*" {
        tracing::warn!(
            "GEWU_CORS_ALLOW_ORIGIN='*' rejected (would allow any origin); falling back to strict same-origin"
        );
        return base;
    }
    let origins: Vec<HeaderValue> = trimmed
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| HeaderValue::from_str(s).ok())
        .collect();
    if origins.is_empty() {
        return base;
    }
    base.allow_origin(AllowOrigin::list(origins))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_legacy_tachyon_configs_default_for_plugin_suites() {
        let browser = parse_tachyon_browser_config(serde_json::Value::Null).unwrap();
        let suite = parse_tachyon_plugin_suite_config(serde_json::Value::Null).unwrap();

        assert!(!browser.enabled);
        assert!(!suite.enabled);
    }

    #[test]
    fn content_security_policy_allows_configured_tachyon_browser_url() {
        let assets = RuntimeFrontendAssets {
            plugins: vec![RuntimeFrontendPlugin {
                id: "tachyon".to_string(),
                sidecars: vec![RuntimeFrontendSidecar {
                    id: "browser".to_string(),
                    url: "http://127.0.0.1:8400".to_string(),
                }],
                styles: Vec::new(),
                scripts: Vec::new(),
            }],
        };
        let csp = build_content_security_policy(&assets);
        let csp = csp.to_str().unwrap();

        assert!(csp.contains("connect-src 'self' http://127.0.0.1:8400"));
    }

    #[test]
    fn content_security_policy_allows_plugin_asset_origins() {
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
        let csp = build_content_security_policy(&assets);
        let csp = csp.to_str().unwrap();

        assert!(csp.contains(
            "script-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net http://127.0.0.1:8400"
        ));
        assert!(csp.contains("style-src 'self' 'unsafe-inline' http://127.0.0.1:8400"));
        assert!(csp.contains("connect-src 'self' http://127.0.0.1:8400"));
    }

    #[test]
    fn browser_runtime_assets_use_same_origin_plugin_proxy_urls() {
        let internal_assets = RuntimeFrontendAssets {
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

        let browser_assets = browser_runtime_frontend_assets(&internal_assets);

        assert_eq!(
            plugin_sidecar_map(&internal_assets)["tachyon"]["browser"],
            "http://127.0.0.1:8400"
        );
        assert_eq!(
            browser_assets.plugins[0].sidecars[0].url,
            "/api/plugins/tachyon/browser"
        );
        assert_eq!(
            browser_assets.plugins[0].styles[0],
            "/api/plugins/tachyon/browser/assets/tachyon-canvas.css"
        );
        assert_eq!(
            browser_assets.plugins[0].scripts[0],
            "/api/plugins/tachyon/browser/assets/tachyon-canvas.iife.js"
        );
    }
}
