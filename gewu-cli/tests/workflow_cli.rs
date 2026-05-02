//! CLI-level integration tests for `--workflow` and `--list-workflows`.
//!
//! These drive `run_parsed_with_plugins` with a tool-only workflow (no LLM
//! required) pointed at a temp directory. We override the SQLite DB path and
//! pass dummy credentials so the OpenAI client is constructed but never
//! called.

use gewu_cli::config::Config;
use gewu_cli::{Cli, LoadedConfig, run_parsed_with_plugins};
use gewu_core::traits::PluginPlatformContext;
use gewu_core::traits::{AssemblyContext, Plugin, Tool, ToolFactory};
use gewu_core::types::{SpecialistDescriptor, ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde_json::Value;
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The test plugin registers an `echo` capability — enough to run tool-only
/// workflows without a real LLM.
struct EchoPlugin;
struct EchoFactory;
struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echoes input"
    }
    fn schema(&self) -> RootSchema {
        RootSchema::default()
    }
    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        Ok(ToolResult::new(
            "echo",
            serde_json::to_string(&args).unwrap_or_default(),
        ))
    }
}

impl ToolFactory for EchoFactory {
    fn capabilities(&self) -> Vec<String> {
        vec!["echo".into()]
    }
    fn create_tools(
        &self,
        _cap: &str,
        _ctx: &AssemblyContext,
    ) -> Result<Vec<Box<dyn Tool>>, Box<dyn Error + Send + Sync>> {
        Ok(vec![Box::new(EchoTool)])
    }
}

impl Plugin for EchoPlugin {
    fn name(&self) -> &str {
        "echo"
    }
    fn tool_factories(&self) -> Vec<Box<dyn ToolFactory>> {
        vec![Box::new(EchoFactory)]
    }
    fn specialist_descriptors(&self) -> Vec<SpecialistDescriptor> {
        vec![]
    }
}

fn unique_db_path(tag: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "gewu-cli-wftest-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("gewu.db").to_string_lossy().to_string()
}

fn write_workflow(dir: &std::path::Path, name: &str, yaml: &str) {
    std::fs::write(dir.join(format!("{}.yaml", name)), yaml).unwrap();
}

fn minimal_cli(workflow_dir: &str) -> Cli {
    Cli {
        prompt: None,
        base_url: None,
        api_key: Some("dummy".into()),
        endpoint: Some("http://localhost:9".into()),
        model: Some("dummy".into()),
        max_output_tokens: None,
        agent: "build".into(),
        log_level: "warn".into(),
        list_agents: false,
        list_models: false,
        session_id: Some("t".into()),
        db_path: Some(unique_db_path("cli")),
        llm_config: None,
        workflow: None,
        workflow_input: None,
        workflow_dirs: vec![workflow_dir.to_string()],
        list_workflows: false,
    }
}

#[tokio::test]
async fn list_workflows_prints_loaded_ids() {
    let td = tempfile::tempdir().unwrap();
    write_workflow(
        td.path(),
        "first",
        "id: first\ndescription: alpha\nentry: a\nfinish: [a]\nnodes: { a: { kind: tool, tool: echo } }\n",
    );
    write_workflow(
        td.path(),
        "second",
        "id: second\ndescription: beta\nentry: a\nfinish: [a]\nnodes: { a: { kind: tool, tool: echo } }\n",
    );

    let mut cli = minimal_cli(td.path().to_str().unwrap());
    cli.list_workflows = true;

    let loaded = LoadedConfig {
        config: Config::default(),
        path: None,
    };
    let r = run_parsed_with_plugins(cli, loaded, |reg| {
        reg.register("echo", |_ctx: &PluginPlatformContext| {
            Ok(Box::new(EchoPlugin) as Box<dyn Plugin>)
        });
    })
    .await;
    assert!(r.is_ok(), "list_workflows failed: {:?}", r.err());
}

#[tokio::test]
async fn run_workflow_invokes_tool_only_graph() {
    let td = tempfile::tempdir().unwrap();
    write_workflow(
        td.path(),
        "single",
        "id: single\nentry: a\nfinish: [a]\nnodes: { a: { kind: tool, tool: echo, write: out } }\n",
    );

    let mut cli = minimal_cli(td.path().to_str().unwrap());
    cli.workflow = Some("single".into());
    cli.workflow_input = Some(r#"{"k":42}"#.into());

    // Enable the echo plugin via config.plugins so the runtime registers it.
    let mut cfg = Config::default();
    cfg.plugins.insert("echo".into(), serde_json::json!({}));
    let loaded = LoadedConfig {
        config: cfg,
        path: None,
    };
    let r = run_parsed_with_plugins(cli, loaded, |reg| {
        reg.register("echo", |_ctx: &PluginPlatformContext| {
            Ok(Box::new(EchoPlugin) as Box<dyn Plugin>)
        });
    })
    .await;
    assert!(r.is_ok(), "run_workflow failed: {:?}", r.err());
}

#[tokio::test]
async fn run_workflow_errors_on_unknown_id() {
    let td = tempfile::tempdir().unwrap();

    let mut cli = minimal_cli(td.path().to_str().unwrap());
    cli.workflow = Some("does_not_exist".into());

    let loaded = LoadedConfig {
        config: Config::default(),
        path: None,
    };
    let r = run_parsed_with_plugins(cli, loaded, |reg| {
        reg.register("echo", |_ctx: &PluginPlatformContext| {
            Ok(Box::new(EchoPlugin) as Box<dyn Plugin>)
        });
    })
    .await;
    assert!(r.is_err());
    let msg = r.unwrap_err().to_string();
    assert!(msg.contains("not found"), "got: {}", msg);
}

#[tokio::test]
async fn workflow_input_from_file_is_read() {
    let td = tempfile::tempdir().unwrap();
    write_workflow(
        td.path(),
        "inp",
        "id: inp\nentry: a\nfinish: [a]\nnodes: { a: { kind: tool, tool: echo, write: out } }\n",
    );
    let input_path = td.path().join("in.json");
    std::fs::write(&input_path, r#"{"from":"file"}"#).unwrap();

    let mut cli = minimal_cli(td.path().to_str().unwrap());
    cli.workflow = Some("inp".into());
    cli.workflow_input = Some(format!("@{}", input_path.display()));

    let mut cfg = Config::default();
    cfg.plugins.insert("echo".into(), serde_json::json!({}));
    let loaded = LoadedConfig {
        config: cfg,
        path: None,
    };
    let r = run_parsed_with_plugins(cli, loaded, |reg| {
        reg.register("echo", |_ctx: &PluginPlatformContext| {
            Ok(Box::new(EchoPlugin) as Box<dyn Plugin>)
        });
    })
    .await;
    assert!(r.is_ok(), "file input workflow failed: {:?}", r.err());
}
