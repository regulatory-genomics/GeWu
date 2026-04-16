//! Real-API validation for the external specialist plugin system.
//!
//! Run: cargo run --example validate_plugin

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use gewu_cli::config::LoadedConfig;
use gewu_core::traits::{AssemblyContext, LlmProvider, Plugin, Tool, ToolFactory};
use gewu_core::types::{AgentEvent, SpecialistDescriptor, ToolContext, ToolResult};
use gewu_providers::OpenAIProvider;
use gewu_runtime::GeWuRuntime;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use sqlx::sqlite::SqlitePoolOptions;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Deserialize, JsonSchema)]
struct CountLinesArgs {
    path: String,
}

struct CountLinesTool {
    working_directory: PathBuf,
}

#[async_trait]
impl Tool for CountLinesTool {
    fn name(&self) -> &str {
        "count_lines"
    }

    fn description(&self) -> &str {
        "Count lines in a file"
    }

    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(CountLinesArgs)
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let params: CountLinesArgs = serde_json::from_value(args)?;
        let path = self.working_directory.join(&params.path);
        let content = std::fs::read_to_string(&path)?;
        Ok(ToolResult::new(
            "count_lines",
            content.lines().count().to_string(),
        ))
    }
}

struct ExampleToolFactory;

impl ToolFactory for ExampleToolFactory {
    fn capabilities(&self) -> Vec<String> {
        vec!["count_lines".into()]
    }

    fn create_tools(
        &self,
        capability: &str,
        ctx: &AssemblyContext,
    ) -> Result<Vec<Box<dyn Tool>>, Box<dyn Error + Send + Sync>> {
        match capability {
            "count_lines" => Ok(vec![Box::new(CountLinesTool {
                working_directory: ctx.working_directory.clone(),
            })]),
            _ => Ok(vec![]),
        }
    }
}

struct ExamplePlugin;

impl Plugin for ExamplePlugin {
    fn name(&self) -> &str {
        "example"
    }

    fn tool_factories(&self) -> Vec<Box<dyn ToolFactory>> {
        vec![Box::new(ExampleToolFactory)]
    }

    fn specialist_descriptors(&self) -> Vec<SpecialistDescriptor> {
        vec![SpecialistDescriptor {
            id: "line-counter".into(),
            description: "Counts lines in files".into(),
            capabilities: vec![
                "read_file".into(),
                "count_lines".into(),
                "report_result".into(),
            ],
            system_prompt:
                "You count lines in files. Use count_lines and call report_result when done.".into(),
        }]
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let loaded = LoadedConfig::load(None)?;
    let cfg = &loaded.config;

    let api_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .or(cfg.api_key.clone())
        .context("OPENAI_API_KEY not set")?;
    let endpoint = std::env::var("OPENAI_ENDPOINT")
        .ok()
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
        .or(cfg.endpoint.clone())
        .context("OPENAI_ENDPOINT not set")?;
    let model = std::env::var("OPENAI_MODEL")
        .ok()
        .or(cfg.model.clone())
        .unwrap_or_else(|| "gpt-4o-mini".to_string());

    let provider: Arc<dyn LlmProvider> = Arc::new(OpenAIProvider::new_with_endpoint(
        &api_key, &endpoint, &model,
    ));
    let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;

    let runtime = GeWuRuntime::builder()
        .provider(provider)
        .pool(pool)
        .register_builtin_capabilities()
        .register_builtin_specialists()
        .register_plugin(Box::new(ExamplePlugin))
        .build()?;

    println!("=== External Plugin Validation ===");
    println!("Model: {model}");
    println!();

    let (mut agent, _) = runtime
        .build_specialist("line-counter", "plugin-bench".to_string(), vec![])
        .await?;

    let prompt = "How many lines are in Cargo.toml? Reply with the number only.";
    println!("Prompt: {prompt}");
    println!("--- agent output ---");

    let mut stream = Box::pin(agent.ask(prompt));
    let mut tool_count = 0u32;
    let mut result = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(AgentEvent::ContentDelta { delta }) => {
                print!("{delta}");
                result.push_str(&delta);
            }
            Ok(AgentEvent::ToolStart { tool, .. }) => {
                tool_count += 1;
                println!("  [tool call #{tool_count}] {tool}");
            }
            Ok(AgentEvent::Finish { response }) => result = response,
            Ok(AgentEvent::Error { error }) => println!("  [error] {error}"),
            Err(e) => {
                println!("  [stream error] {e}");
                break;
            }
            _ => {}
        }
    }

    println!();
    println!("--- end ---");
    println!();
    println!("[RESULT] tools used: {tool_count}, response: {result:?}");

    Ok(())
}
