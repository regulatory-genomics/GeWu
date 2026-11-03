//! Shared startup logic used by both `run` and `serve` subcommands.
//!
//! Resolves configuration, initializes the LLM provider, opens the SQLite
//! pool, discovers skills, and opens the optional LLM request log.

use anyhow::{Context, Result};
use gewu_core::traits::{LlmProvider, RequestLog};
use gewu_providers::{AnthropicProvider, OpenAIProvider};
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;
use std::env;
use std::sync::{Arc, Mutex};
use tracing::info;
use uuid::Uuid;

use crate::config::LoadedConfig;

/// CLI flags shared by the `run` and `serve` subcommands.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct CommonArgs {
    /// Custom OpenAI Base URL
    #[arg(long, env = "OPENAI_BASE_URL")]
    pub base_url: Option<String>,

    /// OpenAI API Key
    #[arg(long, env = "OPENAI_API_KEY")]
    pub api_key: Option<String>,

    /// OpenAI Endpoint URL
    #[arg(long, env = "OPENAI_ENDPOINT")]
    pub endpoint: Option<String>,

    /// Custom Model Name
    #[arg(long, short, env = "OPENAI_MODEL")]
    pub model: Option<String>,

    /// LLM Provider to use (openai or anthropic)
    #[arg(long, env = "GEWU_PROVIDER")]
    pub provider: Option<String>,

    /// Initial Agent (default: build)
    #[arg(long, default_value = "build")]
    pub agent: String,

    /// Log level
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// Path to SQLite database file
    #[arg(long)]
    pub db_path: Option<String>,

    /// Path to LLM configuration file
    #[arg(long)]
    pub llm_config: Option<String>,
}

/// Everything needed to build agents, resolved from CLI args + config + env.
pub struct Bootstrap {
    pub provider: Arc<dyn LlmProvider>,
    pub pool: SqlitePool,
    pub mcp_tools: Vec<Arc<dyn gewu_core::traits::Tool>>,
    pub skills: Vec<gewu_tools::SkillInfo>,
    /// Destination policy shared by every `webfetch` instance this process builds.
    pub web_fetch: Arc<gewu_tools::WebFetchPolicy>,
    pub log_writer: Option<RequestLog>,
    pub model: String,
    pub provider_name: String,
}

/// Initialize tracing from the `--log-level` flag.
pub fn init_tracing(log_level: &str) -> Result<()> {
    let level = match log_level.to_lowercase().as_str() {
        "error" => tracing::Level::ERROR,
        "warn" => tracing::Level::WARN,
        "info" => tracing::Level::INFO,
        "debug" => tracing::Level::DEBUG,
        "trace" => tracing::Level::TRACE,
        _ => tracing::Level::INFO,
    };
    let subscriber = tracing_subscriber::fmt().with_max_level(level).finish();
    tracing::subscriber::set_global_default(subscriber)?;
    Ok(())
}

/// Resolve the SQLite database path from CLI args or the default location.
pub fn resolve_db_path(db_path: Option<String>) -> Result<String> {
    if let Some(path) = db_path {
        return Ok(path);
    }
    let home_dir = dirs::home_dir()
        .context("Failed to determine home directory. Please provide --db-path explicitly.")?;
    let gewu_dir = home_dir.join(".gewu");

    if !gewu_dir.exists() {
        std::fs::create_dir_all(&gewu_dir).context("Failed to create .gewu directory")?;
    }

    Ok(gewu_dir.join("gewu.db").to_string_lossy().to_string())
}

/// Resolve the list of skill directories from config, making relative paths
/// absolute against the current working directory.
pub fn resolve_skill_paths(config: &crate::config::Config) -> Vec<std::path::PathBuf> {
    config
        .skills
        .iter()
        .flatten()
        .map(|p| {
            let path = std::path::PathBuf::from(p);
            if path.is_absolute() {
                path
            } else {
                env::current_dir()
                    .map(|dir| dir.join(path))
                    .unwrap_or_else(|_| std::path::PathBuf::from(p))
            }
        })
        .collect()
}

/// Resolve provider/model settings and build the LLM provider.
///
/// Priority: CLI args > environment variables > config file > defaults.
pub fn build_provider(
    args: &CommonArgs,
    file_config: &crate::config::Config,
) -> Result<(Arc<dyn LlmProvider>, String, String)> {
    let api_key = args
        .api_key
        .clone()
        .or_else(|| env::var("OPENAI_API_KEY").ok())
        .or(file_config.api_key.clone())
        .context(
            "OPENAI_API_KEY not provided. Set via --api-key, OPENAI_API_KEY env var, or config.json",
        )?;

    let base_url = args
        .base_url
        .clone()
        .or_else(|| env::var("OPENAI_BASE_URL").ok())
        .or(args.endpoint.clone())
        .or_else(|| env::var("OPENAI_ENDPOINT").ok())
        .or(file_config.base_url.clone());

    let provider_name = args
        .provider
        .clone()
        .or_else(|| env::var("GEWU_PROVIDER").ok())
        .or_else(|| file_config.provider.clone())
        .unwrap_or_else(|| "openai".to_string());

    let model = args
        .model
        .clone()
        .or_else(|| env::var("OPENAI_MODEL").ok())
        .or(file_config.model.clone())
        .unwrap_or_else(|| "gpt-5.3-codex".to_string());

    let provider: Arc<dyn LlmProvider> = match provider_name.to_lowercase().as_str() {
        "anthropic" => {
            if let Some(url) = base_url {
                Arc::new(AnthropicProvider::new_with_endpoint(&api_key, &url, &model))
            } else {
                Arc::new(AnthropicProvider::new(&api_key, &model))
            }
        }
        _ => {
            if let Some(url) = base_url {
                Arc::new(OpenAIProvider::new_with_endpoint(&api_key, &url, &model))
            } else {
                Arc::new(OpenAIProvider::new(&api_key, &model))
            }
        }
    };

    Ok((provider, provider_name, model))
}

/// Open the optional LLM request log file when configured.
pub fn open_request_log(file_config: &crate::config::Config) -> Result<Option<RequestLog>> {
    let Some(path) = &file_config.log else {
        return Ok(None);
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Failed to open LLM request log file: {}", path))?;
    info!("Logging LLM requests to: {}", path);
    Ok(Some(Arc::new(Mutex::new(Box::new(file)))))
}

/// Perform the full startup sequence shared by `run` and `serve`.
pub async fn bootstrap(args: &CommonArgs) -> Result<Bootstrap> {
    init_tracing(&args.log_level)?;

    let loaded_config = LoadedConfig::load(args.llm_config.as_deref())?;
    if let Some(path) = &loaded_config.path {
        info!("Loaded config from: {}", path.display());
    } else {
        info!("No config file found");
    }
    let file_config = &loaded_config.config;

    let (provider, provider_name, model) = build_provider(args, file_config)?;
    info!("Using model: {}", model);
    info!("Using provider: {}", provider_name);

    let db_path = resolve_db_path(args.db_path.clone())?;
    info!("Using database: {}", db_path);
    let db_url = format!("sqlite://{}?mode=rwc", db_path);
    let pool = SqlitePoolOptions::new()
        .connect(&db_url)
        .await
        .context("Failed to connect to SQLite database")?;

    let mcp_tools =
        crate::mcp_loader::load_mcp_tools(loaded_config.config.mcp_servers.clone()).await?;
    let log_writer = open_request_log(file_config)?;

    let skill_paths = resolve_skill_paths(file_config);
    let skills = if skill_paths.is_empty() {
        Vec::new()
    } else {
        let found = gewu_tools::skill::discover(&skill_paths);
        info!("Discovered {} skills", found.len());
        found
    };

    let web_fetch = Arc::new(
        file_config
            .web_fetch
            .as_ref()
            .map(|config| config.policy())
            .unwrap_or_default(),
    );
    if web_fetch.block_private_networks {
        info!("webfetch: private networks are blocked");
    }

    Ok(Bootstrap {
        provider,
        pool,
        mcp_tools,
        skills,
        web_fetch,
        log_writer,
        model,
        provider_name,
    })
}

/// Default session ID generation shared by subcommands.
pub fn new_session_id() -> String {
    Uuid::new_v4().to_string()
}
