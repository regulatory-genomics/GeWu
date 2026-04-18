pub mod config;
pub mod mcp_loader;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use gewu_core::traits::LlmProvider;
use gewu_core::types::AgentEvent;
use gewu_providers::OpenAIProvider;
use gewu_runtime::{PluginHost, PluginRegistry, RuntimeFactory};
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
use std::collections::HashMap;
use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};
use uuid::Uuid;

pub use config::LoadedConfig;
pub use gewu_runtime::{
    AgentConfig, BuiltinToolFactory, GeWuRuntime, GeWuRuntimeBuilder, TaskTool, create_agent,
    find_descriptor, list_available_agents, resolve_tools, specialist_descriptors,
};

const DIM_GRAY: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";

#[derive(Parser, Debug, Clone)]
#[command(name = "gewu-cli")]
#[command(version = "0.1.0")]
#[command(about = "GeWu CLI Agent", long_about = None)]
pub struct Cli {
    /// Initial prompt to start the session
    #[arg(trailing_var_arg = true)]
    pub prompt: Option<Vec<String>>,

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

    /// Initial Agent (default: build)
    #[arg(long, default_value = "build")]
    pub agent: String,

    /// Log level
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// List available agents
    #[arg(long)]
    pub list_agents: bool,

    /// List available models from the provider
    #[arg(long)]
    pub list_models: bool,

    /// Session ID for persistent memory (optional, generated if not provided)
    #[arg(long)]
    pub session_id: Option<String>,

    /// Path to SQLite database file
    #[arg(long)]
    pub db_path: Option<String>,

    /// Path to LLM configuration file
    #[arg(long)]
    pub llm_config: Option<String>,
}

async fn run_agent(agent: &mut gewu_core::Agent, prompt: &str) -> Result<()> {
    let mut stream = Box::pin(agent.ask(prompt));
    let mut reasoning = String::new();
    let mut content = String::new();
    let mut reasoning_started = false;
    while let Some(event_result) = stream.next().await {
        match event_result {
            Ok(event) => match event {
                AgentEvent::ContentDelta { delta } => {
                    print!("{}", delta);
                    std::io::stdout().flush()?;
                    content.push_str(&delta);
                }
                AgentEvent::ReasoningDelta { delta } => {
                    if !reasoning_started {
                        reasoning_started = true;
                        print!("{}[thinking] ", DIM_GRAY);
                    }
                    print!("{}{}", delta, RESET);
                    std::io::stdout().flush()?;
                    reasoning.push_str(&delta);
                }
                AgentEvent::ToolStart { tool, input } => {
                    println!("\n[Tool Start: {}] Input: {}", tool, input);
                }
                AgentEvent::ToolFinish { tool, output } => {
                    let display_output = if output.len() > 200 {
                        format!("{}...", &output[..200])
                    } else {
                        output
                    };
                    println!("\n[Tool Finish: {}] Output: {}", tool, display_output);
                }
                AgentEvent::Finish { .. } => {
                    println!();
                }
                AgentEvent::Error { error } => {
                    error!("Error from agent: {}", error);
                }
            },
            Err(e) => error!("Stream error: {}", e),
        }
    }
    Ok(())
}

pub fn build_plugin_host_with<F>(
    plugins: &HashMap<String, serde_json::Value>,
    provider: Arc<dyn LlmProvider>,
    pool: SqlitePool,
    working_directory: PathBuf,
    extra_register: F,
) -> Result<PluginHost>
where
    F: FnOnce(&mut PluginRegistry),
{
    let mut plugin_registry = PluginRegistry::new();
    extra_register(&mut plugin_registry);
    PluginHost::from_config(plugins, &plugin_registry, provider, pool, working_directory)
}

async fn print_available_agents<F>(loaded_config: &LoadedConfig, extra_register: F) -> Result<()>
where
    F: FnOnce(&mut PluginRegistry),
{
    let provider: Arc<dyn LlmProvider> = Arc::new(OpenAIProvider::new_with_endpoint(
        "dummy",
        "http://localhost",
        "dummy",
    ));
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .context("Failed to create in-memory SQLite database for agent listing")?;
    let working_directory = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let plugin_host = build_plugin_host_with(
        &loaded_config.config.plugins,
        provider.clone(),
        pool.clone(),
        working_directory,
        extra_register,
    )?;
    let runtime = RuntimeFactory::build(provider, pool, &plugin_host)?;

    println!("Available Agents:");
    for agent in runtime.list_specialists() {
        println!("  - {}: {}", agent.name, agent.description);
    }

    if !loaded_config.config.plugins.is_empty() {
        println!(
            "\nConfigured plugins: {:?}",
            loaded_config.config.plugins.keys().collect::<Vec<_>>()
        );
    }

    Ok(())
}

pub async fn run_with_plugins<F>(extra_register: F) -> Result<()>
where
    F: FnOnce(&mut PluginRegistry),
{
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let loaded_config = LoadedConfig::load(cli.llm_config.as_deref())?;
    run_parsed_with_plugins(cli, loaded_config, extra_register).await
}

pub async fn run_parsed_with_plugins<F>(
    cli: Cli,
    loaded_config: LoadedConfig,
    extra_register: F,
) -> Result<()>
where
    F: FnOnce(&mut PluginRegistry),
{
    let log_level = match cli.log_level.to_lowercase().as_str() {
        "error" => tracing::Level::ERROR,
        "warn" => tracing::Level::WARN,
        "info" => tracing::Level::INFO,
        "debug" => tracing::Level::DEBUG,
        "trace" => tracing::Level::TRACE,
        _ => tracing::Level::INFO,
    };

    let subscriber = tracing_subscriber::fmt().with_max_level(log_level).finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    if let Some(path) = &loaded_config.path {
        info!("Loaded config from: {}", path.display());
    } else {
        info!("No config file found");
    }

    if cli.list_agents {
        return print_available_agents(&loaded_config, extra_register).await;
    }

    let file_config = loaded_config.config;

    let api_key = cli
        .api_key
        .or_else(|| env::var("OPENAI_API_KEY").ok())
        .or(file_config.api_key.clone())
        .context(
            "OPENAI_API_KEY not provided. Set via --api-key, OPENAI_API_KEY env var, or config.json",
        )?;

    let endpoint = cli
        .endpoint
        .or_else(|| env::var("OPENAI_ENDPOINT").ok())
        .or(cli.base_url.clone())
        .or_else(|| env::var("OPENAI_BASE_URL").ok())
        .or(file_config.endpoint.clone())
        .context(
            "Endpoint not provided. Set via --endpoint, --base-url, OPENAI_ENDPOINT/OPENAI_BASE_URL env var, or config.json",
        )?;

    if cli.list_models {
        let provider = OpenAIProvider::new_with_endpoint(&api_key, &endpoint, "dummy");
        match provider.list_models().await {
            Ok(models) => {
                println!("Available Models:");
                for model in models {
                    println!("  - {}", model);
                }
            }
            Err(e) => {
                eprintln!("Error listing models: {}", e);
            }
        }
        return Ok(());
    }

    let model = cli
        .model
        .or_else(|| env::var("OPENAI_MODEL").ok())
        .or(file_config.model.clone())
        .unwrap_or_else(|| "gpt-5.3-codex".to_string());

    info!("Using model: {}", model);

    let provider: Arc<dyn LlmProvider> = Arc::new(OpenAIProvider::new_with_endpoint(
        &api_key, &endpoint, &model,
    ));

    let db_path = if let Some(path) = cli.db_path {
        path
    } else {
        let home_dir = dirs::home_dir()
            .context("Failed to determine home directory. Please provide --db-path explicitly.")?;
        let gewu_dir = home_dir.join(".gewu");

        if !gewu_dir.exists() {
            std::fs::create_dir_all(&gewu_dir).context("Failed to create .gewu directory")?;
        }

        gewu_dir.join("gewu.db").to_string_lossy().to_string()
    };

    info!("Using database: {}", db_path);
    let db_url = format!("sqlite://{}?mode=rwc", db_path);

    let pool = SqlitePoolOptions::new()
        .connect(&db_url)
        .await
        .context("Failed to connect to SQLite database")?;

    let session_id = cli
        .session_id
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    println!("Session ID: {}", session_id);

    let mcp_tools = mcp_loader::load_mcp_tools(file_config.mcp_servers).await?;
    let plugin_host = build_plugin_host_with(
        &file_config.plugins,
        provider.clone(),
        pool.clone(),
        env::current_dir()?,
        extra_register,
    )?;
    let runtime = RuntimeFactory::build(provider.clone(), pool.clone(), &plugin_host)?;
    let (mut agent, _registry) = runtime
        .build_specialist(&cli.agent, session_id, mcp_tools)
        .await?;
    info!("Initialized agent: {}", cli.agent);

    if let Some(prompt_parts) = cli.prompt {
        let prompt = prompt_parts.join(" ");
        if !prompt.is_empty() {
            println!("> {}", prompt);
            if let Err(e) = run_agent(&mut agent, &prompt).await {
                error!("Error: {}", e);
            }
        }
    }

    use std::io::{self, Write as _};
    println!("--- GeWu CLI (Agent: {}) ---", cli.agent);
    println!("Type 'exit' or 'quit' to end.");

    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();

        if input.is_empty() {
            continue;
        }

        if input == "exit" || input == "quit" {
            break;
        }

        if let Err(e) = run_agent(&mut agent, input).await {
            error!("Error processing message: {}", e);
        }
    }

    Ok(())
}
