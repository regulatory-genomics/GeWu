use anyhow::{Context, Result};
use clap::Parser;
use gewu_providers::OpenAIProvider;
use serde::Deserialize;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

mod agent;
mod config;
mod document;
mod retrieval;
mod tools;

use agent::graph::RagSystem;
use config::Config;
use document::indexer::DocumentIndexer;

#[derive(Parser)]
#[command(name = "gewu-rag")]
#[command(version = "0.1.0")]
#[command(about = "GeWu RAG - Retrieval Augmented Generation System", long_about = None)]
struct Cli {
    #[arg(short, long)]
    query: Option<String>,

    #[arg(short, long)]
    index: Option<String>,

    #[arg(long)]
    recreate_index: bool,

    #[arg(long)]
    interactive: bool,

    #[arg(long)]
    config: Option<String>,

    #[arg(long, env = "QDRANT_URL")]
    qdrant_url: Option<String>,

    #[arg(long, env = "QDRANT_API_KEY")]
    qdrant_api_key: Option<String>,

    #[arg(long, env = "PARENT_STORE_PATH")]
    parent_store_path: Option<String>,

    #[arg(long, default_value = "document_child_chunks")]
    collection: String,

    #[arg(long, default_value_t = 500)]
    chunk_size: usize,

    #[arg(long, default_value_t = 100)]
    chunk_overlap: usize,

    #[arg(long, default_value_t = 2000)]
    min_parent_size: usize,

    #[arg(long, default_value_t = 4000)]
    max_parent_size: usize,

    #[arg(long, default_value_t = 10)]
    max_iterations: usize,

    #[arg(long, default_value_t = 8)]
    max_tool_calls: usize,

    #[arg(long, default_value_t = 0.7)]
    similarity_threshold: f64,

    #[arg(long, env = "OPENAI_BASE_URL")]
    base_url: Option<String>,

    #[arg(long, env = "OPENAI_API_KEY")]
    api_key: Option<String>,

    #[arg(long, default_value = "gpt-4.1")]
    model: String,

    #[arg(long, default_value = "info")]
    log_level: String,

    #[arg(long, default_value = "markdown_docs")]
    docs_dir: String,

    #[arg(long, default_value = "parent_store")]
    output_dir: String,

    #[arg(long, default_value_t = false)]
    enable_sparse: bool,
}

#[derive(Debug, Deserialize, Default)]
struct ConfigFile {
    #[serde(rename = "api_key")]
    api_key: Option<String>,
    #[serde(rename = "base_url")]
    base_url: Option<String>,
    #[serde(rename = "model")]
    model: Option<String>,
    #[serde(rename = "qdrant_url")]
    qdrant_url: Option<String>,
    #[serde(rename = "qdrant_api_key")]
    qdrant_api_key: Option<String>,
    #[serde(rename = "parent_store_path")]
    parent_store_path: Option<String>,
    #[serde(rename = "collection")]
    collection: Option<String>,
    #[serde(rename = "chunk_size")]
    chunk_size: Option<usize>,
    #[serde(rename = "chunk_overlap")]
    chunk_overlap: Option<usize>,
    #[serde(rename = "min_parent_size")]
    min_parent_size: Option<usize>,
    #[serde(rename = "max_parent_size")]
    max_parent_size: Option<usize>,
    #[serde(rename = "max_iterations")]
    max_iterations: Option<usize>,
    #[serde(rename = "max_tool_calls")]
    max_tool_calls: Option<usize>,
    #[serde(rename = "similarity_threshold")]
    similarity_threshold: Option<f64>,
    #[serde(rename = "docs_dir")]
    docs_dir: Option<String>,
    #[serde(rename = "output_dir")]
    output_dir: Option<String>,
    #[serde(rename = "enable_sparse")]
    enable_sparse: Option<bool>,
    #[serde(rename = "sparse_model")]
    sparse_model: Option<String>,
}

fn strip_jsonc_comments(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut in_string = false;
    let mut chars = content.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '"' && chars.peek() != Some(&'"') {
            in_string = !in_string;
            result.push(c);
        } else if in_string {
            result.push(c);
        } else if c == '/' && chars.peek() == Some(&'/') {
            while chars.peek() != Some(&'\n') && chars.next().is_some() {}
        } else {
            result.push(c);
        }
    }
    result
}

fn load_config_file(path: &str) -> Result<ConfigFile> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path))?;

    let json_content = strip_jsonc_comments(&content);

    let config: ConfigFile = serde_json::from_str(&json_content)
        .with_context(|| format!("Failed to parse config file: {}", path))?;

    Ok(config)
}

fn build_config(cli: &Cli, config_file: &Option<ConfigFile>) -> Config {
    let mut config = Config::new();

    if let Some(cf) = config_file {
        config.qdrant_url = cf
            .qdrant_url
            .clone()
            .unwrap_or_else(|| "http://localhost:6334".to_string());
        config.qdrant_api_key = cf.qdrant_api_key.clone();
        config.parent_store_path = cf
            .parent_store_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("parent_store"));
        config.child_collection = cf
            .collection
            .clone()
            .unwrap_or_else(|| "document_child_chunks".to_string());
        config.chunk_size = cf.chunk_size.unwrap_or(500);
        config.chunk_overlap = cf.chunk_overlap.unwrap_or(100);
        config.min_parent_size = cf.min_parent_size.unwrap_or(2000);
        config.max_parent_size = cf.max_parent_size.unwrap_or(4000);
        config.max_iterations = cf.max_iterations.unwrap_or(10);
        config.max_tool_calls = cf.max_tool_calls.unwrap_or(8);
        config.similarity_threshold = cf.similarity_threshold.unwrap_or(0.7);
        config.docs_dir = cf
            .docs_dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("markdown_docs"));
        config.parent_store_path = cf
            .output_dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("parent_store"));
        config.enable_sparse = cf.enable_sparse.unwrap_or(false);
        config.sparse_model = cf
            .sparse_model
            .clone()
            .unwrap_or_else(|| "prithivida/Splade_PP_en_v1".to_string());
    }

    if let Some(ref url) = cli.qdrant_url {
        config.qdrant_url = url.clone();
    }

    if let Some(ref key) = cli.qdrant_api_key {
        config.qdrant_api_key = Some(key.clone());
    }

    if let Some(ref path) = cli.parent_store_path {
        config.parent_store_path = PathBuf::from(path);
    }

    config.child_collection = cli.collection.clone();
    config.chunk_size = cli.chunk_size;
    config.chunk_overlap = cli.chunk_overlap;
    config.min_parent_size = cli.min_parent_size;
    config.max_parent_size = cli.max_parent_size;
    config.max_iterations = cli.max_iterations;
    config.max_tool_calls = cli.max_tool_calls;
    config.similarity_threshold = cli.similarity_threshold;
    config.docs_dir = PathBuf::from(&cli.docs_dir);
    config.parent_store_path = PathBuf::from(&cli.output_dir);
    config.enable_sparse = cli.enable_sparse;

    config
}

async fn index_documents(cli: &Cli, config: &Config) -> Result<()> {
    let mut indexer = DocumentIndexer::new(config.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create indexer: {}", e))?;

    if cli.recreate_index {
        info!("Recreating collection...");
        indexer
            .recreate_collection()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to recreate collection: {}", e))?;
    }

    let docs_dir = config.docs_dir.clone();
    if !docs_dir.exists() {
        anyhow::bail!("Docs directory does not exist: {:?}", docs_dir);
    }

    info!("Indexing documents from {:?}", docs_dir);
    indexer
        .index_markdown_dir(&docs_dir)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to index documents: {}", e))?;
    info!("Indexing complete!");

    Ok(())
}

async fn run_interactive(rag_system: &RagSystem) -> Result<()> {
    println!("=== GeWu RAG Interactive Mode ===");
    println!("Type 'exit' or 'quit' to end.");
    println!("Type 'clear' to clear conversation history.");
    println!();

    loop {
        print!("> ");
        std::io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();

        if input.is_empty() {
            continue;
        }

        if input == "exit" || input == "quit" {
            break;
        }

        if input == "clear" {
            println!("Note: Conversation history clearing not yet implemented.");
            continue;
        }

        print!("\n");
        match rag_system.query(input).await {
            Ok(response) => {
                println!("\n=== Answer ===\n{}\n", response);
            }
            Err(e) => {
                eprintln!("Query error: {}", e);
            }
        }
    }

    Ok(())
}

async fn run_query(rag_system: &RagSystem, query: &str) -> Result<()> {
    info!("Query: {}", query);
    match rag_system.query(query).await {
        Ok(response) => {
            println!("\n=== Answer ===\n{}\n", response);
        }
        Err(e) => {
            anyhow::bail!("Query failed: {}", e);
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

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

    let config_file = if let Some(ref path) = cli.config {
        Some(load_config_file(path)?)
    } else {
        None
    };

    let config = build_config(&cli, &config_file);

    let api_key = cli
        .api_key
        .clone()
        .or_else(|| config_file.as_ref().and_then(|cf| cf.api_key.clone()))
        .or_else(|| env::var("OPENAI_API_KEY").ok())
        .context("OPENAI_API_KEY not provided. Set via --api-key, config file, or OPENAI_API_KEY env var")?;

    let endpoint = cli
        .base_url
        .clone()
        .or_else(|| config_file.as_ref().and_then(|cf| cf.base_url.clone()))
        .or_else(|| env::var("OPENAI_BASE_URL").ok())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

    let model = config_file
        .as_ref()
        .and_then(|cf| cf.model.clone())
        .unwrap_or_else(|| cli.model.clone());

    info!("Using model: {}", model);
    info!("Using endpoint: {}", endpoint);

    let llm: Arc<dyn gewu_core::LlmProvider> = Arc::new(OpenAIProvider::new_with_endpoint(
        &api_key, &endpoint, &model,
    ));

    if let Some(ref index_dir) = cli.index {
        let mut index_config = config.clone();
        index_config.docs_dir = PathBuf::from(index_dir);
        index_documents(&cli, &index_config).await?;
    } else if cli.recreate_index {
        anyhow::bail!("--recreate-index requires --index");
    } else if cli.interactive {
        let rag_system = RagSystem::new(config.clone(), llm)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create RAG system: {}", e))?;
        run_interactive(&rag_system).await?;
    } else if cli.query.is_some() {
        let rag_system = RagSystem::new(config.clone(), llm)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create RAG system: {}", e))?;
        run_query(&rag_system, cli.query.as_ref().unwrap()).await?;
    } else {
        anyhow::bail!(
            "Either --query, --interactive, or --index must be provided. Use --help for usage."
        );
    }

    Ok(())
}
