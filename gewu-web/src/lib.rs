use anyhow::{bail, Context, Result};
use axum::{
    middleware,
    routing::{delete, get, post},
    Router,
};
use clap::Parser;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

pub mod auth;
pub mod auth_middleware;
pub mod fallback;
pub mod routes;
pub mod sidecar;
pub mod state;

use auth::AuthMode;
use gewu_cli::mcp_loader;
use gewu_runtime::PluginRegistry;
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
}

pub async fn run_with_plugins<F>(register_plugins: F) -> Result<()>
where
    F: Fn(&mut PluginRegistry) + Send + Sync + 'static,
{
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

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

    let tachyon_browser_cfg: sidecar::TachyonBrowserConfig =
        serde_json::from_value(loaded_config.config.tachyon_browser.clone())
            .context("Failed to parse tachyonBrowser config")?;

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
        runtime_plugin_dirs: loaded_config.config.runtime_plugin_dirs,
        skill_dirs: loaded_config.config.skill_dirs,
        tachyon_browser_url: env_tachyon_url.clone(),
        tachyon_browser: tachyon_browser_cfg,
    };

    let mcp_tools = mcp_loader::load_mcp_tools(mcp_servers).await?;

    let managed_tachyon_browser =
        sidecar::ensure_tachyon_browser(&config.tachyon_browser, env_tachyon_url.as_deref())
            .await?;
    if let Some(sidecar) = managed_tachyon_browser.as_ref() {
        config.tachyon_browser_url = Some(sidecar.url().to_string());
        // SAFETY: set before any thread spawns and reads env. Single-threaded init.
        unsafe {
            std::env::set_var("TACHYON_BROWSER_URL", "");
        }
    }

    let auth_mode = AuthMode::from_cli(cli.password.as_deref())?;
    if !cli.host.is_loopback() && !auth_mode.requires_password() && !cli.allow_insecure_bind {
        bail!(
            "non-loopback bind ({}) requires --password or --allow-insecure-bind",
            cli.host
        );
    }
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
        .route("/api/chat", post(routes::chat::handle_chat))
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
            "/api/browser/{*path}",
            get(routes::canvas::handle_browser_get).post(routes::canvas::handle_browser_post),
        )
        .route_layer(middleware::from_fn_with_state(
            app_state.clone(),
            auth_middleware::require_auth,
        ));

    // Static file serving via ServeDir (handles path traversal correctly).
    // Resolved relative to the project's gewu-web/static dir (current
    // working directory at run time, matching previous behaviour).
    let static_service = ServeDir::new("gewu-web/static")
        .append_index_html_on_directories(false)
        .precompressed_gzip();

    let app = public
        .merge(protected)
        .fallback_service(static_service)
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::header::CONTENT_SECURITY_POLICY,
            axum::http::HeaderValue::from_static(
                "default-src 'self'; img-src 'self' data:; \
                 script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; \
                 connect-src 'self'; font-src 'self' data:",
            ),
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

/// Build the CORS layer. By default disallows cross-origin requests so the
/// local-dev server is not silently reachable by other web pages on the
/// user's machine. Honors `GEWU_CORS_ALLOW_ORIGIN` (comma-separated origins)
/// when an explicit allowlist is needed.
fn build_cors_layer() -> CorsLayer {
    use axum::http::{header, HeaderValue, Method};

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
