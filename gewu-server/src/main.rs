pub mod bootstrap;
pub mod config;
pub mod mcp_loader;
mod memory_view;
pub mod registry;
pub mod server;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use futures::StreamExt;
use gewu_core::traits::LlmProvider;
use gewu_core::types::AgentEvent;
use std::env;
use std::io::{Read, Write};
use tracing::{error, info};

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[90m";
const SPINNER_FRAMES: [&str; 6] = ["·", "✢", "✳", "∗", "✻", "✽"];

/// Extracts the exact command from a `bash` tool call's JSON arguments.
fn extract_bash_command(input: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(input)
        .ok()?
        .get("command")?
        .as_str()
        .map(str::to_string)
}

/// Clear the spinner line (if shown) and stop animating it.
fn stop_spinner(spinner_active: &mut bool, is_tty: bool) -> Result<()> {
    if *spinner_active {
        *spinner_active = false;
        if is_tty {
            print!("\r\x1b[K");
            std::io::stdout().flush()?;
        }
    }
    Ok(())
}

async fn run_agent(agent: &mut gewu_core::Agent, prompt: &str) -> Result<()> {
    let is_tty = atty::is(atty::Stream::Stdout);

    let mut stream = Box::pin(agent.ask(prompt));
    let mut spinner_active = false;
    let mut spinner_frame: usize = 0;
    let mut content_started = false;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tick.tick(), if spinner_active && is_tty => {
                print!(
                    "\r\x1b[K{}{} thinking…{}",
                    DIM,
                    SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()],
                    RESET
                );
                spinner_frame += 1;
                std::io::stdout().flush()?;
            }
            event_result = stream.next() => {
                let Some(event_result) = event_result else { break };
                match event_result {
                    Ok(event) => match event {
                        AgentEvent::ContentDelta { delta } => {
                            stop_spinner(&mut spinner_active, is_tty)?;
                            if !content_started {
                                content_started = true;
                                println!();
                            }
                            print!("{}", delta);
                            std::io::stdout().flush()?;
                        }
                        AgentEvent::ReasoningDelta { .. } => {
                            if !spinner_active {
                                spinner_active = true;
                                spinner_frame = 0;
                            }
                        }
                        AgentEvent::ToolStart { tool, input, .. } => {
                            stop_spinner(&mut spinner_active, is_tty)?;
                            if tool == "bash"
                                && let Some(command) = extract_bash_command(&input)
                            {
                                println!("\n[Tool Start: bash] $ {}", command);
                            } else {
                                println!("\n[Tool Start: {}]", tool);
                            }
                        }
                        AgentEvent::ToolFinish { tool, output, .. } => {
                            stop_spinner(&mut spinner_active, is_tty)?;
                            if output.starts_with("Error:") {
                                eprintln!("\n[Tool Error: {}] {}", tool, output);
                            }
                        }
                        AgentEvent::Finish { response: _ } => {
                            stop_spinner(&mut spinner_active, is_tty)?;
                            println!();
                        }
                        AgentEvent::Cancelled { reason } => {
                            stop_spinner(&mut spinner_active, is_tty)?;
                            eprintln!("\n[cancelled] {}", reason);
                        }
                        AgentEvent::Error { error } => {
                            stop_spinner(&mut spinner_active, is_tty)?;
                            error!("Error from agent: {}", error);
                        }
                    },
                    Err(e) => error!("Stream error: {}", e),
                }
            }
        }
    }

    stop_spinner(&mut spinner_active, is_tty)?;

    Ok(())
}

#[derive(Parser)]
#[command(name = "gewu-server")]
#[command(version = "0.1.0")]
#[command(about = "GeWu agent CLI and server", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run an agent with a prompt or in interactive mode
    Run(RunArgs),
    /// Run an agent as a server other programs can connect to
    Serve(ServeArgs),
    /// View persisted agent memory
    View(ViewArgs),
}

#[derive(Args)]
struct RunArgs {
    /// Prompt to send (without -i flag, runs once and exits)
    #[arg(trailing_var_arg = true)]
    prompt: Option<Vec<String>>,

    #[command(flatten)]
    common: bootstrap::CommonArgs,

    /// List available agents
    #[arg(long)]
    list_agents: bool,

    /// List available models from the provider
    #[arg(long)]
    list_models: bool,

    /// Session ID for persistent memory (optional, generated if not provided)
    #[arg(long)]
    session_id: Option<String>,

    /// Run in interactive mode
    #[arg(short, long)]
    interactive: bool,
}

#[derive(Args)]
struct ServeArgs {
    /// Host to bind both listeners to
    #[arg(long, default_value = server::DEFAULT_HOST)]
    host: String,

    /// Port for the WebSocket listener
    #[arg(long, default_value_t = server::DEFAULT_WS_PORT)]
    ws_port: u16,

    /// Port for the newline-delimited JSON TCP listener
    #[arg(long, default_value_t = server::DEFAULT_TCP_PORT)]
    tcp_port: u16,

    #[command(flatten)]
    common: bootstrap::CommonArgs,
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
        Command::Serve(args) => serve(args).await,
        Command::View(args) => {
            memory_view::execute(args.session_id.as_deref(), args.db_path.as_deref()).await
        }
    }
}

async fn serve(cli: ServeArgs) -> Result<()> {
    let boot = bootstrap::bootstrap(&cli.common).await?;
    info!(
        "Starting gewu server (agent: {}, model: {}, provider: {})",
        cli.common.agent, boot.model, boot.provider_name
    );
    server::serve(boot, &cli.host, cli.ws_port, cli.tcp_port).await
}

async fn run(cli: RunArgs) -> Result<()> {
    // Initialize logging
    bootstrap::init_tracing(&cli.common.log_level)?;

    if cli.list_agents {
        println!("Available Agents:");
        for agent in registry::list_available_agents() {
            println!("  - {}: {}", agent.name, agent.description);
        }
        return Ok(());
    }

    let loaded_config = config::LoadedConfig::load(cli.common.llm_config.as_deref())?;

    if let Some(path) = &loaded_config.path {
        info!("Loaded config from: {}", path.display());
    } else {
        info!("No config file found");
    }

    let file_config = &loaded_config.config;

    if cli.list_models {
        let model = cli
            .common
            .model
            .clone()
            .or_else(|| env::var("OPENAI_MODEL").ok())
            .or(file_config.model.clone())
            .unwrap_or_else(|| "gpt-5.3-codex".to_string());
        let url = cli
            .common
            .base_url
            .clone()
            .or_else(|| env::var("OPENAI_BASE_URL").ok())
            .or(cli.common.endpoint.clone())
            .or_else(|| env::var("OPENAI_ENDPOINT").ok())
            .or(file_config.base_url.clone())
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let api_key = cli
            .common
            .api_key
            .clone()
            .or_else(|| env::var("OPENAI_API_KEY").ok())
            .or(file_config.api_key.clone())
            .context(
                "OPENAI_API_KEY not provided. Set via --api-key, OPENAI_API_KEY env var, or config.json",
            )?;
        let provider = gewu_providers::OpenAIProvider::new_with_endpoint(&api_key, &url, &model);
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

    let boot = bootstrap::bootstrap(&cli.common).await?;
    info!("Using model: {}", boot.model);
    info!("Using provider: {}", boot.provider_name);

    // Determine Session ID
    let session_id = cli.session_id.unwrap_or_else(bootstrap::new_session_id);

    // Create Agent
    let (mut agent, _registry) = registry::create_agent(
        &cli.common.agent,
        boot.provider,
        boot.pool,
        session_id,
        boot.mcp_tools,
        boot.log_writer,
        boot.skills,
    )
    .await?;
    info!("Initialized agent: {}", cli.common.agent);

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
        println!("--- GeWu CLI (Agent: {}) ---", cli.common.agent);
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
        let cli = Cli::try_parse_from(["gewu-server", "run", "hello", "world"]).unwrap();
        match cli.command {
            Command::Run(args) => {
                assert_eq!(args.prompt, Some(vec!["hello".into(), "world".into()]))
            }
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn parses_view_commands() {
        let list = Cli::try_parse_from(["gewu-server", "view"]).unwrap();
        match list.command {
            Command::View(args) => assert_eq!(args.session_id, None),
            _ => panic!("expected view command"),
        }

        let show = Cli::try_parse_from(["gewu-server", "view", "session-1"]).unwrap();
        match show.command {
            Command::View(args) => assert_eq!(args.session_id.as_deref(), Some("session-1")),
            _ => panic!("expected view command"),
        }
    }

    #[test]
    fn parses_serve_command_with_defaults() {
        let cli = Cli::try_parse_from(["gewu-server", "serve"]).unwrap();
        match cli.command {
            Command::Serve(args) => {
                assert_eq!(args.host, server::DEFAULT_HOST);
                assert_eq!(args.ws_port, server::DEFAULT_WS_PORT);
                assert_eq!(args.tcp_port, server::DEFAULT_TCP_PORT);
                assert_eq!(args.common.agent, "build");
            }
            _ => panic!("expected serve command"),
        }
    }

    #[test]
    fn parses_serve_command_with_overrides() {
        let cli = Cli::try_parse_from([
            "gewu-server",
            "serve",
            "--host",
            "0.0.0.0",
            "--ws-port",
            "9000",
            "--tcp-port",
            "9001",
            "--agent",
            "explore",
        ])
        .unwrap();
        match cli.command {
            Command::Serve(args) => {
                assert_eq!(args.host, "0.0.0.0");
                assert_eq!(args.ws_port, 9000);
                assert_eq!(args.tcp_port, 9001);
                assert_eq!(args.common.agent, "explore");
            }
            _ => panic!("expected serve command"),
        }
    }

    #[test]
    fn rejects_legacy_top_level_prompt() {
        assert!(Cli::try_parse_from(["gewu-server", "hello"]).is_err());
    }

    #[test]
    fn extracts_bash_command_from_tool_input() {
        let input = r#"{"command":"git status --short","timeout":5000}"#;
        assert_eq!(
            extract_bash_command(input),
            Some("git status --short".to_string())
        );
    }

    #[test]
    fn bash_command_extraction_ignores_invalid_input() {
        assert_eq!(extract_bash_command("not json"), None);
        assert_eq!(extract_bash_command(r#"{"timeout":1000}"#), None);
        assert_eq!(extract_bash_command(r#"{"command":42}"#), None);
    }
}
