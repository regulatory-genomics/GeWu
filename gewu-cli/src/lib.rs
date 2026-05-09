pub mod config;
pub mod evolve_tools;
pub mod mcp_loader;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{AgentEvent, SpecialistExecutionRequest};
use gewu_graph::Blackboard;
use gewu_providers::OpenAIProvider;
use gewu_runtime::{
    PluginHost, PluginRegistry, RuntimeFactory, SpecialistExecutionEvent, WorkflowCompiler,
    WorkflowRegistry,
};
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
use std::collections::HashMap;
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
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

    /// Maximum model output tokens for each LLM request
    #[arg(long = "max-output-tokens", env = "GEWU_MAX_OUTPUT_TOKENS")]
    pub max_output_tokens: Option<u32>,

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

    /// Working directory for tools and runtime plugins. Defaults to the process cwd.
    #[arg(long)]
    pub workdir: Option<String>,

    /// Path to LLM configuration file
    #[arg(long)]
    pub llm_config: Option<String>,

    /// Run a declarative workflow by id (from `--workflow-dir` or
    /// `~/.gewu/workflows/`). When set, the interactive agent loop is skipped.
    #[arg(long)]
    pub workflow: Option<String>,

    /// Input for `--workflow`. Either inline JSON (`{"k":1}`) or `@path.json`.
    /// Defaults to `{}` (empty object) when omitted.
    #[arg(long)]
    pub workflow_input: Option<String>,

    /// Additional directory to scan for workflow manifests. May be repeated.
    #[arg(long = "workflow-dir")]
    pub workflow_dirs: Vec<String>,

    /// List available workflows and exit.
    #[arg(long)]
    pub list_workflows: bool,
}

async fn run_runtime_agent(
    runtime: &gewu_runtime::GeWuRuntime,
    agent_id: &str,
    session_id: &str,
    additional_tools: &[Arc<dyn Tool>],
    prompt: &str,
    workdir: &Path,
) -> Result<()> {
    let request = SpecialistExecutionRequest {
        task_id: Uuid::new_v4().to_string(),
        specialist_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        prompt: prompt.to_string(),
        workdir: Some(workdir.to_string_lossy().to_string()),
        metadata_json: serde_json::json!({
            "user_id": "default",
            "workdir": workdir.to_string_lossy().to_string(),
            "workspace_root": workdir.to_string_lossy().to_string(),
        }),
    };
    let mut stream = runtime.execute_request_stream(request, additional_tools.to_vec());
    let mut reasoning_started = false;
    while let Some(event_result) = stream.next().await {
        match event_result? {
            SpecialistExecutionEvent::Agent(event) => match event {
                AgentEvent::ContentDelta { delta } => {
                    print!("{delta}");
                    std::io::stdout().flush()?;
                }
                AgentEvent::ReasoningDelta { delta } => {
                    if !reasoning_started {
                        reasoning_started = true;
                        print!("{DIM_GRAY}[thinking] ");
                    }
                    print!("{delta}{RESET}");
                    std::io::stdout().flush()?;
                }
                AgentEvent::ToolStart { tool, input, .. } => {
                    println!("\n[Tool Start: {tool}] Input: {input}");
                }
                AgentEvent::ToolFinish { tool, output, .. } => {
                    let display_output = if output.len() > 200 {
                        let end = output
                            .char_indices()
                            .map(|(i, _)| i)
                            .take_while(|&i| i <= 200)
                            .last()
                            .unwrap_or(0);
                        format!("{}...", &output[..end])
                    } else {
                        output
                    };
                    println!("\n[Tool Finish: {tool}] Output: {display_output}");
                }
                AgentEvent::ToolPermissionRequest(request) => {
                    println!(
                        "\n[Permission Request: {}] {} ({})",
                        request.tool_name,
                        request.requested_path.display(),
                        request.reason
                    );
                }
                AgentEvent::ToolPermissionResolved { request_id, approved } => {
                    println!("\n[Permission Resolved: {request_id}] approved={approved}");
                }
                AgentEvent::Finish { .. } => {
                    println!();
                }
                AgentEvent::Error { error } => {
                    error!("Error from agent: {}", error);
                }
            },
            SpecialistExecutionEvent::Completed(envelope) => {
                if envelope.output_json.is_none()
                    && let Some(reason) = envelope.block_reason
                {
                    println!("{reason}");
                }
            }
        }
    }
    Ok(())
}

fn resolve_manifest_dirs(config: &config::Config) -> Vec<PathBuf> {
    let home = dirs::home_dir();
    let mut dirs: Vec<PathBuf> = config
        .specialist_dirs
        .iter()
        .map(|s| {
            if s.starts_with("~/")
                && let Some(ref h) = home
            {
                return h.join(&s[2..]);
            }
            PathBuf::from(s)
        })
        .collect();
    if dirs.is_empty() {
        if let Some(ref h) = home {
            dirs.push(h.join(".gewu").join("specialists"));
        }
        dirs.push(PathBuf::from(".gewu/specialists"));
    }
    dirs
}

fn resolve_workflow_dirs(config: &config::Config, cli_extra: &[String]) -> Vec<PathBuf> {
    let home = dirs::home_dir();
    let expand = |s: &String| -> PathBuf {
        if s.starts_with("~/")
            && let Some(ref h) = home
        {
            return h.join(&s[2..]);
        }
        PathBuf::from(s)
    };
    let mut dirs: Vec<PathBuf> = config.workflow_dirs.iter().map(expand).collect();
    for s in cli_extra {
        dirs.push(expand(s));
    }
    if dirs.is_empty() {
        if let Some(ref h) = home {
            dirs.push(h.join(".gewu").join("workflows"));
        }
        dirs.push(PathBuf::from(".gewu/workflows"));
    }
    dirs
}

fn expand_config_path(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(s)
}

fn resolve_cli_workdir(workdir: Option<&str>) -> Result<PathBuf> {
    let path = match workdir {
        Some(value) => PathBuf::from(value),
        None => env::current_dir()?,
    };
    let absolute = if path.is_absolute() {
        path
    } else {
        env::current_dir()?.join(path)
    };
    absolute
        .canonicalize()
        .with_context(|| format!("resolving --workdir {}", absolute.display()))
}

fn resolve_runtime_plugin_dirs(config: &config::Config) -> Vec<PathBuf> {
    config
        .runtime_plugin_dirs
        .iter()
        .map(|s| expand_config_path(s))
        .collect()
}

fn resolve_skill_dirs(config: &config::Config) -> Vec<PathBuf> {
    config
        .skill_dirs
        .iter()
        .map(|s| expand_config_path(s))
        .collect()
}

fn parse_workflow_input(raw: Option<&str>) -> Result<serde_json::Value> {
    let s = match raw {
        None => return Ok(serde_json::json!({})),
        Some(s) => s,
    };
    if let Some(path) = s.strip_prefix('@') {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading workflow input file: {path}"))?;
        serde_json::from_str(&content).with_context(|| format!("parsing JSON from {path}"))
    } else {
        serde_json::from_str(s).with_context(|| "parsing --workflow-input as JSON")
    }
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
    gewu_storage::run_migrations(&pool).await?;
    let working_directory = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let plugin_host = build_plugin_host_with(
        &loaded_config.config.plugins,
        provider.clone(),
        pool.clone(),
        working_directory,
        extra_register,
    )?;
    let runtime = RuntimeFactory::build_with_manifest_and_runtime_plugin_dirs(
        provider,
        pool,
        &plugin_host,
        &resolve_manifest_dirs(&loaded_config.config),
        &resolve_runtime_plugin_dirs(&loaded_config.config),
        &resolve_skill_dirs(&loaded_config.config),
        &resolve_workflow_dirs(&loaded_config.config, &[]),
    )?;

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

async fn print_available_workflows<F>(
    loaded_config: &LoadedConfig,
    cli_dirs: &[String],
    extra_register: F,
) -> Result<()>
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
        .context("Failed to create in-memory SQLite database for workflow listing")?;
    gewu_storage::run_migrations(&pool).await?;
    let working_directory = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let plugin_host = build_plugin_host_with(
        &loaded_config.config.plugins,
        provider,
        pool,
        working_directory,
        extra_register,
    )?;
    let dirs = resolve_workflow_dirs(&loaded_config.config, cli_dirs);
    let registry = WorkflowRegistry::from_sources(&plugin_host, &dirs)?;

    println!("Available Workflows:");
    for id in registry.ids() {
        let m = registry.get(&id).unwrap();
        let desc = if m.description.is_empty() {
            "—"
        } else {
            &m.description
        };
        println!("  - {id}: {desc}");
    }
    Ok(())
}

async fn run_workflow(
    runtime: gewu_runtime::GeWuRuntime,
    registry: WorkflowRegistry,
    workflow_id: &str,
    input: serde_json::Value,
) -> Result<()> {
    let manifest = registry
        .get(workflow_id)
        .with_context(|| {
            format!(
                "Workflow '{}' not found. Available: {:?}",
                workflow_id,
                registry.ids()
            )
        })?
        .clone();
    let compiler = WorkflowCompiler::new(Arc::new(runtime)).with_workflows(registry.into_map());
    let graph = compiler.compile(&manifest)?;
    let bb = Blackboard::from_value(input);
    let out = graph.ainvoke(bb).await?;
    let pretty = serde_json::to_string_pretty(out.as_value())?;
    println!("{pretty}");
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

    if cli.list_workflows {
        return print_available_workflows(&loaded_config, &cli.workflow_dirs, extra_register).await;
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
        .or(file_config.base_url.clone())
        .context(
            "Endpoint not provided. Set via --endpoint, --base-url, OPENAI_ENDPOINT/OPENAI_BASE_URL env var, or config.json",
        )?;

    if cli.list_models {
        let provider = OpenAIProvider::new_with_endpoint(&api_key, &endpoint, "dummy");
        match provider.list_models().await {
            Ok(models) => {
                println!("Available Models:");
                for model in models {
                    println!("  - {model}");
                }
            }
            Err(e) => {
                eprintln!("Error listing models: {e}");
            }
        }
        return Ok(());
    }

    let model = cli
        .model
        .or_else(|| env::var("OPENAI_MODEL").ok())
        .or(file_config.model.clone())
        .unwrap_or_else(|| "gpt-5.3-codex".to_string());

    let max_output_tokens = cli
        .max_output_tokens
        .or_else(|| {
            env::var("GEWU_MAX_OUTPUT_TOKENS")
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
        })
        .or(file_config.max_output_tokens);

    info!("Using model: {}", model);

    let provider: Arc<dyn LlmProvider> = Arc::new(
        OpenAIProvider::new_with_endpoint(&api_key, &endpoint, &model)
            .with_max_output_tokens(max_output_tokens),
    );

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

    let pool = gewu_storage::create_pool(Path::new(&db_path)).await?;
    gewu_storage::run_migrations(&pool).await?;

    let session_id = cli.session_id.unwrap_or_else(|| Uuid::new_v4().to_string());
    println!("Session ID: {session_id}");

    let working_directory = resolve_cli_workdir(cli.workdir.as_deref())?;

    let manifest_dirs = resolve_manifest_dirs(&file_config);
    let runtime_plugin_dirs = resolve_runtime_plugin_dirs(&file_config);
    let skill_dirs = resolve_skill_dirs(&file_config);
    let workflow_dirs = resolve_workflow_dirs(&file_config, &cli.workflow_dirs);
    let mcp_tools = mcp_loader::load_mcp_tools(file_config.mcp_servers).await?;
    let plugin_host = build_plugin_host_with(
        &file_config.plugins,
        provider.clone(),
        pool.clone(),
        working_directory.clone(),
        extra_register,
    )?;
    let runtime = RuntimeFactory::build_with_manifest_and_runtime_plugin_dirs_in_directory(
        provider.clone(),
        pool.clone(),
        &plugin_host,
        &manifest_dirs,
        &runtime_plugin_dirs,
        &skill_dirs,
        &workflow_dirs,
        &working_directory,
    )?;

    if let Some(workflow_id) = cli.workflow.as_deref() {
        let input = parse_workflow_input(cli.workflow_input.as_deref())?;
        let registry = WorkflowRegistry::from_sources(&plugin_host, &workflow_dirs)?;
        return run_workflow(runtime, registry, workflow_id, input).await;
    }

    info!("Initialized agent: {}", cli.agent);

    if let Some(prompt_parts) = cli.prompt {
        let prompt = prompt_parts.join(" ");
        if !prompt.is_empty() {
            println!("> {prompt}");
            if let Err(e) = run_runtime_agent(
                &runtime,
                &cli.agent,
                &session_id,
                &mcp_tools,
                &prompt,
                &working_directory,
            )
            .await
            {
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

        if let Err(e) = run_runtime_agent(
            &runtime,
            &cli.agent,
            &session_id,
            &mcp_tools,
            input,
            &working_directory,
        )
        .await
        {
            error!("Error processing message: {}", e);
        }
    }

    Ok(())
}
