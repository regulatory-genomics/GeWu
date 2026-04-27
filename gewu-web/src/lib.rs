use anyhow::{Context, Result};
use axum::{
    Router,
    routing::{delete, get, post},
};
use clap::Parser;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

pub mod auth;
pub mod fallback;
pub mod routes;
pub mod state;

use auth::AuthState;
use gewu_cli::mcp_loader;
use gewu_runtime::PluginRegistry;
use state::{AppState, PluginRegistrar};

#[derive(Parser, Debug)]
#[command(name = "gewu-web")]
#[command(version = "0.1.0")]
#[command(about = "GeWu Web UI - Browser-based interface for the GeWu agent framework")]
struct Cli {
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

    #[arg(long)]
    db_path: Option<String>,
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
        .unwrap_or_else(|| "gpt-5.3-codex".to_string());

    let db_path = cli.db_path.or_else(|| {
        dirs::home_dir().map(|h| {
            h.join(".gewu")
                .join("gewu.db")
                .to_string_lossy()
                .to_string()
        })
    });

    let mcp_servers = loaded_config.config.mcp_servers;

    let config = state::ServerConfig {
        api_key: Some(api_key),
        endpoint: Some(endpoint),
        model: Some(model),
        db_path,
        plugins: loaded_config.config.plugins,
    };

    let mcp_tools = mcp_loader::load_mcp_tools(mcp_servers).await?;

    let auth_state = AuthState::new();
    let plugin_registrar: PluginRegistrar = Arc::new(register_plugins);
    let app_state = Arc::new(
        AppState::new(config, auth_state.clone(), mcp_tools, plugin_registrar).await?,
    );

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let token = auth_state.valid_token.clone();

    let app = Router::new()
        .route("/", get(fallback::serve_index))
        .route("/api/validate", get(routes::sessions::handle_validate))
        .route("/api/token", get(routes::sessions::handle_get_token))
        .route("/api/info", get(routes::sessions::handle_server_info))
        .route("/api/agents", get(routes::sessions::handle_list_agents))
        .route("/api/chat", post(routes::chat::handle_chat))
        .route("/api/sessions", get(routes::sessions::handle_list_sessions))
        .route("/api/sessions", post(routes::sessions::handle_create_session))
        .route(
            "/api/sessions/{id}",
            delete(routes::sessions::handle_delete_session),
        )
        .route(
            "/api/sessions/{id}/messages",
            get(routes::sessions::handle_get_messages),
        )
        .route(
            "/api/sessions/{id}/switch-agent",
            post(routes::sessions::handle_switch_agent),
        )
        .route("/index.html", get(fallback::serve_index))
        .fallback(fallback::serve_static)
        .layer(cors)
        .with_state(app_state);

    let addr = SocketAddr::from(([0, 0, 0, 0], cli.port));

    println!("========================================");
    println!("GeWu Web UI");
    println!("========================================");
    println!("URL: http://localhost:{}/?token={}", cli.port, token);
    println!("========================================");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
