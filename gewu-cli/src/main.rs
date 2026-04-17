pub mod config;
pub mod mcp_loader;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use gewu_core::traits::LlmProvider;
use gewu_core::types::AgentEvent;
use gewu_providers::OpenAIProvider;
use gewu_runtime::{RuntimeFactory, PluginHost, PluginRegistry, list_available_agents};
use sqlx::sqlite::SqlitePoolOptions;
use std::env;
use std::io::Write;
use std::sync::Arc;
use tracing::{error, info};
use uuid::Uuid;

const DIM_GRAY: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";

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

#[derive(Parser)]
#[command(name = "gewu-cli")]
#[command(version = "0.1.0")]
#[command(about = "GeWu CLI Agent", long_about = None)]
struct Cli {
    /// Initial prompt to start the session
    #[arg(trailing_var_arg = true)]
    prompt: Option<Vec<String>>,

    /// Custom OpenAI Base URL
    #[arg(long, env = "OPENAI_BASE_URL")]
    base_url: Option<String>,

    /// OpenAI API Key
    #[arg(long, env = "OPENAI_API_KEY")]
    api_key: Option<String>,

    /// OpenAI Endpoint URL
    #[arg(long, env = "OPENAI_ENDPOINT")]
    endpoint: Option<String>,

    /// Custom Model Name
    #[arg(long, short, env = "OPENAI_MODEL")]
    model: Option<String>,

    /// Initial Agent (default: build)
    #[arg(long, default_value = "build")]
    agent: String,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,

    /// List available agents
    #[arg(long)]
    list_agents: bool,

    /// List available models from the provider
    #[arg(long)]
    list_models: bool,

    /// Session ID for persistent memory (optional, generated if not provided)
    #[arg(long)]
    session_id: Option<String>,

    /// Path to SQLite database file
    #[arg(long)]
    db_path: Option<String>,

    /// Path to LLM configuration file
    #[arg(long)]
    llm_config: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    // Initialize logging
    let log_level = match cli.log_level.to_lowercase().as_str() {
        "error" => tracing::Level::ERROR,
        "warn" => tracing::Level::WARN,
        "info" => tracing::Level::INFO,
        "debug" => tracing::Level::DEBUG,
        "trace" => tracing::Level::TRACE,
        _ => tracing::Level::INFO,
    };

    let subscriber = tracing_subscriber::fmt().with_max_level(log_level).finish();

    tracing::subscriber::set_global_default(subscriber)?;

    // Build plugin host early so list_agents can include plugin specialists
    let loaded_config = config::LoadedConfig::load(cli.llm_config.as_deref())?;

    if let Some(path) = &loaded_config.path {
        info!("Loaded config from: {}", path.display());
    } else {
        info!("No config file found");
    }

    let file_config = &loaded_config.config;

    if cli.list_agents {
        println!("Available Agents (builtins):");
        for agent in list_available_agents() {
            println!("  - {}: {}", agent.name, agent.description);
        }
        #[cfg(feature = "genome")]
        {
            println!("\nGenome plugin specialists:");
            for desc in genome_plugin::genome_specialist_descriptors() {
                println!("  - {}: {}", desc.id, desc.description);
            }
        }
        if !file_config.plugins.is_empty() {
            println!("\nConfigured plugins: {:?}", file_config.plugins.keys().collect::<Vec<_>>());
        }
        return Ok(());
    }

    let api_key = cli.api_key
        .or_else(|| env::var("OPENAI_API_KEY").ok())
        .or(file_config.api_key.clone())
        .context("OPENAI_API_KEY not provided. Set via --api-key, OPENAI_API_KEY env var, or config.json")?;

    let endpoint = cli.endpoint
        .or_else(|| env::var("OPENAI_ENDPOINT").ok())
        .or(cli.base_url.clone())
        .or_else(|| env::var("OPENAI_BASE_URL").ok())
        .or(file_config.endpoint.clone())
        .context("Endpoint not provided. Set via --endpoint, --base-url, OPENAI_ENDPOINT/OPENAI_BASE_URL env var, or config.json")?;

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

    let base_url = Some(endpoint);

    // Initialize Provider
    let provider = if let Some(url) = base_url {
        Arc::new(OpenAIProvider::new_with_endpoint(&api_key, &url, &model))
    } else {
        Arc::new(OpenAIProvider::new(&api_key, &model))
    };

    // Initialize SQLite Pool
    let db_path = if let Some(path) = cli.db_path {
        path
    } else {
        let home_dir = dirs::home_dir()
            .context("Failed to determine home directory. Please provide --db-path explicitly.")?;
        let gewu_dir = home_dir.join(".gewu");

        // Ensure .gewu directory exists
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

    // Determine Session ID
    let session_id = cli.session_id.unwrap_or_else(|| Uuid::new_v4().to_string());
    println!("Session ID: {}", session_id);

    // Load MCP tools
    let mcp_tools = mcp_loader::load_mcp_tools(loaded_config.config.mcp_servers).await?;

    // Build runtime with plugin host
    #[allow(unused_mut)]
    let mut plugin_registry = PluginRegistry::new();

    // Register genome plugin when compiled with the genome feature
    #[cfg(feature = "genome")]
    plugin_registry.register("genome", genome_plugin::build_genome_plugin);

    let plugin_host = PluginHost::from_config(
        &loaded_config.config.plugins,
        &plugin_registry,
        provider.clone(),
        pool.clone(),
        std::env::current_dir()?,
    )?;
    let runtime = RuntimeFactory::build(provider.clone(), pool.clone(), &plugin_host)?;
    let (mut agent, _registry) = runtime
        .build_specialist(&cli.agent, session_id, mcp_tools)
        .await?;
    info!("Initialized agent: {}", cli.agent);

    // Initial Prompt handling
    if let Some(prompt_parts) = cli.prompt {
        let prompt = prompt_parts.join(" ");
        if !prompt.is_empty() {
            println!("> {}", prompt);
            if let Err(e) = run_agent(&mut agent, &prompt).await {
                error!("Error: {}", e);
            }
        }
    }

    // Interactive Loop
    use std::io::{self, Write};
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
