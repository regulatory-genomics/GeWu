use anyhow::{Context, Result};
use axum::{Router, routing::get};
use clap::{Parser, Subcommand};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

pub mod catalog;
pub mod config;
pub mod inspector;
pub mod routes;
pub mod scheduler;
pub mod state;

use state::AppState;

#[derive(Parser, Debug)]
#[command(name = "gewu-gateway")]
#[command(version = "0.1.0")]
#[command(about = "GeWu background orchestration gateway")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    Serve(ServeArgs),
}

#[derive(Parser, Debug, Clone)]
struct ServeArgs {
    #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
    bind_host: IpAddr,

    #[arg(long, default_value = "7890")]
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

    #[arg(long, default_value_t = false)]
    disable_scheduler: bool,

    #[arg(long)]
    dashboard_dir: Option<String>,
}

impl Default for ServeArgs {
    fn default() -> Self {
        Self {
            bind_host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 7890,
            config: None,
            api_key: None,
            endpoint: None,
            model: None,
            db_path: None,
            disable_scheduler: false,
            dashboard_dir: None,
        }
    }
}

pub async fn run() -> Result<()> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let args = match cli.command {
        Some(Command::Serve(args)) => args,
        None => ServeArgs::default(),
    };
    serve(args).await
}

async fn serve(args: ServeArgs) -> Result<()> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    let config = config::GatewayConfig::load(
        args.config.as_deref(),
        args.api_key,
        args.endpoint,
        args.model,
        args.db_path,
    )?;

    let state = Arc::new(AppState::new(config).await?);
    if !args.disable_scheduler {
        scheduler::worker::spawn_scheduler(state.clone());
    }

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let mut app = Router::new()
        .route("/health", get(routes::health::health))
        .nest("/api/v1", routes::router())
        .layer(cors)
        .with_state(state);

    if let Some(dashboard_dir) = args.dashboard_dir.as_deref() {
        let dashboard_dir = config::expand_config_path(dashboard_dir);
        let index_file = dashboard_dir.join("index.html");
        app = app.nest_service(
            "/dashboard",
            ServeDir::new(&dashboard_dir).fallback(ServeFile::new(index_file)),
        );
        println!("Dashboard: /dashboard -> {}", dashboard_dir.display());
    }

    let addr = SocketAddr::new(args.bind_host, args.port);
    println!("========================================");
    println!("GeWu Gateway");
    println!("========================================");
    println!("URL: http://{}:{}", args.bind_host, args.port);
    println!("========================================");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding gateway listener at {addr}"))?;
    axum::serve(listener, app).await?;
    Ok(())
}
