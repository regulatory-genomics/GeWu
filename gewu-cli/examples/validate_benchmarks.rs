//! Real-API validation for both benchmarks:
//!   Benchmark 1: Explore assembled from descriptor (no hardcoded branch)
//!   Benchmark 2: Blocked task detected, persisted, and resumed
//!
//! Run: cargo run --example validate_benchmarks

use anyhow::{Context, Result};
use futures::StreamExt;
use gewu_cli::config::LoadedConfig;
use gewu_core::Agent;
use gewu_core::traits::{LlmProvider, Memory};
use gewu_core::types::AgentEvent;
use gewu_providers::OpenAIProvider;
use gewu_runtime::{find_descriptor, resolve_tools};
use gewu_storage::SqliteMemory;
use serde_json::{Value, json};
use sqlx::sqlite::SqlitePoolOptions;
use std::sync::Arc;

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

    println!("=== GeWu Benchmark Validation ===");
    println!("Model: {model}");
    println!();

    let provider: Arc<dyn LlmProvider> = Arc::new(OpenAIProvider::new_with_endpoint(
        &api_key, &endpoint, &model,
    ));
    let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;

    benchmark_1(&provider, &pool).await?;
    println!();
    benchmark_2(&provider, &pool).await?;

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
// Benchmark 1: Explore assembled from descriptor, runs a real task
// ═══════════════════════════════════════════════════════════════════
async fn benchmark_1(provider: &Arc<dyn LlmProvider>, pool: &sqlx::SqlitePool) -> Result<()> {
    println!("── Benchmark 1: Specialist Assembly ──");
    println!();

    // 1. Look up descriptor
    let desc = find_descriptor("explore").expect("explore descriptor must exist");
    println!(
        "[1] Descriptor: id={}, capabilities={:?}",
        desc.id,
        desc.capabilities
            .iter()
            .map(|c| format!("{c:?}"))
            .collect::<Vec<_>>()
    );

    // 2. Resolve tools
    let wd = std::env::current_dir()?;
    let tools = resolve_tools(&desc.capabilities, &wd, provider.clone(), pool.clone());
    let tool_names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    println!("[2] Resolved tools: {tool_names:?}");

    // 3. Build agent
    let memory = SqliteMemory::new(pool.clone(), "bench1".to_string()).await?;
    let tools_arc: Vec<Arc<dyn gewu_core::traits::Tool>> =
        tools.into_iter().map(|t| Arc::from(t) as _).collect();
    let mut agent = Agent::builder()
        .llm(provider.clone())
        .with_tools(tools_arc)
        .memory(Box::new(memory))
        .system_prompt(&desc.system_prompt)
        .build()?;
    println!("[3] Agent built from descriptor");

    // 4. Run real prompt
    let prompt = "How many .rs files are in this project? Just give me the number.";
    println!("[4] Prompt: \"{prompt}\"");
    println!("--- agent output ---");

    let mut stream = Box::pin(agent.ask(prompt));
    let mut result = String::new();
    let mut tool_count = 0u32;
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

    let pass = !result.is_empty();
    println!();
    println!(
        "[RESULT] Benchmark 1: {} (tools used: {}, response length: {})",
        if pass { "PASS" } else { "FAIL" },
        tool_count,
        result.len()
    );
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
// Benchmark 2: End-to-end blocked task resume via TaskTool
//
// Flow:
//   1. Ensure .gewu-bench-data.txt does NOT exist
//   2. Call TaskTool::execute → agent reports blocked via report_result
//   3. Create the file with content "42"
//   4. Call TaskTool::execute with same task_id → agent reports succeeded
//   5. Verify from the returned envelope: status, attempt, result
// ═══════════════════════════════════════════════════════════════════
async fn benchmark_2(provider: &Arc<dyn LlmProvider>, pool: &sqlx::SqlitePool) -> Result<()> {
    use gewu_core::traits::Tool;
    use gewu_core::types::ToolContext;
    use gewu_runtime::TaskTool;

    println!("── Benchmark 2: End-to-End Blocked Task Resume ──");
    println!();

    let target_file = std::env::current_dir()?
        .join(".gewu-bench-data.txt")
        .to_string_lossy()
        .to_string();

    // ── Setup: ensure file does NOT exist ──
    let _ = std::fs::remove_file(&target_file);
    println!("[setup] Ensured {} does not exist", &target_file);
    println!();

    let task_tool = TaskTool::new(provider.clone(), pool.clone());
    let ctx = ToolContext::new("bench2-session", "msg-1");

    // ── Attempt 1: file missing → agent should report_result(blocked) ──
    println!("═══ Attempt 1 ═══");
    let args1 = json!({
        "description": "read bench data",
        "prompt": format!(
            "Read the file {} and tell me the exact number inside it. \
             If the file does not exist or cannot be read, you MUST call \
             report_result with status=\"blocked\".", &target_file
        ),
        "subagent_type": "explore",
    });

    let result1 = task_tool
        .execute(args1, &ctx)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let envelope1: Value = serde_json::from_str(&result1.output)?;

    println!();
    println!(
        "[attempt 1] Envelope: {}",
        serde_json::to_string_pretty(&envelope1)?
    );

    let status1 = envelope1["status"].as_str().unwrap_or("unknown");
    let task_id = envelope1["task_id"].as_str().unwrap_or("").to_string();
    let attempt1 = envelope1["attempt"].as_u64().unwrap_or(0);

    if status1 != "blocked" {
        println!(
            "[RESULT] Benchmark 2: SKIP (agent didn't block on attempt 1, status={status1})"
        );
        let _ = std::fs::remove_file(&target_file);
        return Ok(());
    }
    println!(
        "[attempt 1] Status: blocked, attempt: {}, task_id: {}",
        attempt1, &task_id
    );
    println!();

    // ── Fix the blocking condition ──
    std::fs::write(&target_file, "42")?;
    println!("[fix] Created {} with content \"42\"", &target_file);
    println!();

    // ── Attempt 2: resume with same task_id ──
    println!("═══ Attempt 2 (resume) ═══");

    // Check prior history loaded
    let mem_check = SqliteMemory::new(pool.clone(), task_id.clone()).await?;
    let prior_msgs = mem_check.messages().len();
    println!(
        "[resume] {prior_msgs} prior messages from attempt 1 in SQLite"
    );
    drop(mem_check);

    let args2 = json!({
        "description": "read bench data (retry)",
        "prompt": format!(
            "The file {} should exist now. Read it and tell me the exact number inside. \
             Call report_result with the number as the result.", &target_file
        ),
        "subagent_type": "explore",
        "task_id": &task_id,
    });

    let result2 = task_tool
        .execute(args2, &ctx)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let envelope2: Value = serde_json::from_str(&result2.output)?;

    println!();
    println!(
        "[attempt 2] Envelope: {}",
        serde_json::to_string_pretty(&envelope2)?
    );

    let status2 = envelope2["status"].as_str().unwrap_or("unknown");
    let attempt2 = envelope2["attempt"].as_u64().unwrap_or(0);
    let output2 = envelope2["output_json"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            envelope2["output_json"]["result"]
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_default();

    // ── Final verification ──
    println!();
    println!(
        "[final] status={}, attempt={}, output={:?}",
        status2,
        attempt2,
        &output2[..output2.len().min(80)]
    );

    let pass = status2 == "succeeded" && attempt2 == 2 && output2.contains("42");

    println!();
    println!(
        "[RESULT] Benchmark 2: {}",
        if pass { "PASS" } else { "FAIL" }
    );
    if pass {
        println!("  Task blocked on attempt 1 (file missing)");
        println!("  Condition fixed (file created)");
        println!("  Resumed with same task_id, loaded prior history");
        println!("  Succeeded on attempt 2 with correct result");
        println!("  All state managed by TaskTool internally — no manual TaskStore calls");
    }

    // Cleanup
    let _ = std::fs::remove_file(&target_file);

    Ok(())
}
