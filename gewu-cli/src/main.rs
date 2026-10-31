pub mod config;
pub mod mcp_loader;
mod memory_view;
pub mod registry;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use futures::StreamExt;
use gewu_core::traits::{LlmProvider, RequestLog};
use gewu_core::types::AgentEvent;
use gewu_providers::{AnthropicProvider, OpenAIProvider};
use sqlx::sqlite::SqlitePoolOptions;
use std::env;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use tracing::{error, info};
use uuid::Uuid;

const RESET: &str = "\x1b[0m";

async fn run_agent(agent: &mut gewu_core::Agent, prompt: &str) -> Result<()> {
    let mut stream = Box::pin(agent.ask(prompt));
    let mut reasoning_started = false;
    let mut content_started = false;
    while let Some(event_result) = stream.next().await {
        match event_result {
            Ok(event) => match event {
                AgentEvent::ContentDelta { delta } => {
                    if !content_started {
                        content_started = true;
                        if reasoning_started {
                            print!("{}", RESET);
                        }
                        println!();
                    }
                    print!("{}", delta);
                    std::io::stdout().flush()?;
                }
                AgentEvent::ReasoningDelta { delta } => {
                    if !reasoning_started {
                        reasoning_started = true;
                        print!("\x1b[90m[thinking]\x1b[0m ");
                        print!("\x1b[90m");
                    }
                    print!("{}", delta);
                    std::io::stdout().flush()?;
                }
                AgentEvent::ToolStart { .. } => {
                    if reasoning_started {
                        print!("{}", RESET);
                    }
                }
                AgentEvent::ToolFinish { tool, output, .. } => {
                    if output.starts_with("Error:") {
                        eprintln!("\n[Tool Error: {}] {}", tool, output);
                    } else {
                        println!("\n[Tool Finish: {}]", tool);
                    }
                }
                AgentEvent::Finish { response: _ } => {
                    if reasoning_started {
                        print!("{}", RESET);
                    }
                    println!();
                }
                AgentEvent::Cancelled { reason } => {
                    if reasoning_started {
                        print!("{}", RESET);
                    }
                    eprintln!("\n[cancelled] {}", reason);
                }
                AgentEvent::Error { error } => {
                    if reasoning_started {
                        print!("{}", RESET);
                    }
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
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run an agent with a prompt or in interactive mode
    Run(RunArgs),
    /// View persisted agent memory
    View(ViewArgs),
}

#[derive(Args)]
struct RunArgs {
    /// Prompt to send (without -i flag, runs once and exits)
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

    /// LLM Provider to use (openai or anthropic)
    #[arg(long, env = "GEWU_PROVIDER")]
    provider: Option<String>,

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

    /// Run in interactive mode
    #[arg(short, long)]
    interactive: bool,
}

#[derive(Args)]
struct ViewArgs {
    /// Session ID to display; omit to list available sessions
    session_id: Option<String>,

    /// Path to SQLite database file
    #[arg(long)]
    db_path: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    match Cli::parse().command {
        Command::Run(args) => run(args).await,
        Command::View(args) => {
            memory_view::execute(args.session_id.as_deref(), args.db_path.as_deref()).await
        }
    }
}

async fn run(cli: RunArgs) -> Result<()> {
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

    if cli.list_agents {
        println!("Available Agents:");
        for agent in registry::list_available_agents() {
            println!("  - {}: {}", agent.name, agent.description);
        }
        return Ok(());
    }

    let loaded_config = config::LoadedConfig::load(cli.llm_config.as_deref())?;

    if let Some(path) = &loaded_config.path {
        info!("Loaded config from: {}", path.display());
    } else {
        info!("No config file found");
    }

    let file_config = &loaded_config.config;

    let api_key = cli.api_key
        .or_else(|| env::var("OPENAI_API_KEY").ok())
        .or(file_config.api_key.clone())
        .context("OPENAI_API_KEY not provided. Set via --api-key, OPENAI_API_KEY env var, or config.json")?;

    let base_url = cli
        .base_url
        .clone()
        .or_else(|| env::var("OPENAI_BASE_URL").ok())
        .or(cli.endpoint.clone())
        .or_else(|| env::var("OPENAI_ENDPOINT").ok())
        .or(file_config.base_url.clone());

    let provider_name = cli
        .provider
        .or_else(|| env::var("GEWU_PROVIDER").ok())
        .or_else(|| file_config.provider.clone())
        .unwrap_or_else(|| "openai".to_string());

    if cli.list_models {
        let model = cli
            .model
            .or_else(|| env::var("OPENAI_MODEL").ok())
            .or(file_config.model.clone())
            .unwrap_or_else(|| "gpt-5.3-codex".to_string());
        let url = base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let provider = OpenAIProvider::new_with_endpoint(&api_key, &url, &model);
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
    info!("Using provider: {}", provider_name);

    // Initialize Provider
    let provider: Arc<dyn LlmProvider> = match provider_name.to_lowercase().as_str() {
        "anthropic" => {
            if let Some(url) = base_url {
                Arc::new(AnthropicProvider::new_with_endpoint(&api_key, &url, &model))
            } else {
                Arc::new(AnthropicProvider::new(&api_key, &model))
            }
        }
        "openai" | _ => {
            if let Some(url) = base_url {
                Arc::new(OpenAIProvider::new_with_endpoint(&api_key, &url, &model))
            } else {
                Arc::new(OpenAIProvider::new(&api_key, &model))
            }
        }
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

    // Load MCP tools
    let mcp_tools = mcp_loader::load_mcp_tools(loaded_config.config.mcp_servers.clone()).await?;

    // Open the LLM request log file when configured
    let log_writer: Option<RequestLog> = match &file_config.log {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("Failed to open LLM request log file: {}", path))?;
            info!("Logging LLM requests to: {}", path);
            Some(Arc::new(Mutex::new(Box::new(file))))
        }
        None => None,
    };

    // Discover skills from configured paths
    let skill_paths: Vec<std::path::PathBuf> = file_config
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
        .collect();
    let skills = if skill_paths.is_empty() {
        Vec::new()
    } else {
        let found = gewu_tools::skill::discover(&skill_paths);
        info!("Discovered {} skills", found.len());
        found
    };

    // Create Agent
    let (mut agent, _registry) = registry::create_agent(
        &cli.agent,
        provider,
        pool,
        session_id,
        mcp_tools,
        log_writer,
        skills,
    )
    .await?;
    info!("Initialized agent: {}", cli.agent);

    // Determine the prompt: from args, from stdin, or error
    let prompt_from_args = cli.prompt.clone();
    let prompt_from_stdin = if prompt_from_args.is_none() && !atty::is(atty::Stream::Stdin) {
        let mut buffer = String::new();
        std::io::stdin().read_to_string(&mut buffer).ok();
        if buffer.trim().is_empty() {
            None
        } else {
            Some(buffer)
        }
    } else {
        None
    };

    // Validate that we have a prompt or interactive mode
    if prompt_from_args.is_none() && prompt_from_stdin.is_none() && !cli.interactive {
        eprintln!(
            "Error: No prompt provided. Either provide a prompt as argument, pipe content to stdin, or use -i/--interactive for interactive mode."
        );
        std::process::exit(1);
    }

    // Initial Prompt handling
    if let Some(prompt_parts) = &prompt_from_args {
        let prompt = prompt_parts.join(" ");
        if !prompt.is_empty() {
            println!("> {}", prompt);
            if let Err(e) = run_agent(&mut agent, &prompt).await {
                error!("Error: {}", e);
            }
        }
    } else if let Some(stdin_prompt) = &prompt_from_stdin {
        let trimmed = stdin_prompt.trim();
        if !trimmed.is_empty() {
            if trimmed.len() > 100 {
                println!("> {}...", &trimmed[..100]);
            } else {
                println!("> {}", trimmed);
            }
            if let Err(e) = run_agent(&mut agent, trimmed).await {
                error!("Error: {}", e);
            }
        }
    }

    // Interactive Loop (only if -i flag is set)
    if cli.interactive {
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
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_run_command() {
        let cli = Cli::try_parse_from(["gewu-cli", "run", "hello", "world"]).unwrap();
        match cli.command {
            Command::Run(args) => {
                assert_eq!(args.prompt, Some(vec!["hello".into(), "world".into()]))
            }
            Command::View(_) => panic!("expected run command"),
        }
    }

    #[test]
    fn parses_view_commands() {
        let list = Cli::try_parse_from(["gewu-cli", "view"]).unwrap();
        match list.command {
            Command::View(args) => assert_eq!(args.session_id, None),
            Command::Run(_) => panic!("expected view command"),
        }

        let show = Cli::try_parse_from(["gewu-cli", "view", "session-1"]).unwrap();
        match show.command {
            Command::View(args) => assert_eq!(args.session_id.as_deref(), Some("session-1")),
            Command::Run(_) => panic!("expected view command"),
        }
    }

    #[test]
    fn rejects_legacy_top_level_prompt() {
        assert!(Cli::try_parse_from(["gewu-cli", "hello"]).is_err());
    }
}
