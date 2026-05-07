//! Integration tests for the dynamic tool registration pipeline.
//!
//! Tests the full flow: CommandTool → CommandToolRegistry → register_tool →
//! runtime loading → workflow compilation with command-backed tool nodes.

use gewu_core::traits::Tool;
use gewu_core::types::ToolContext;
use gewu_runtime::command_tool::{CommandTool, CommandToolDef};
use gewu_runtime::command_tool_registry::CommandToolRegistry;
use serde_json::json;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use tempfile::TempDir;

fn ctx() -> ToolContext {
    ToolContext::new("test", "test")
}

// ---------------------------------------------------------------------------
// Phase 1: CommandTool spawns processes correctly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn command_tool_roundtrip_json() {
    let def = CommandToolDef {
        name: "json_echo".into(),
        description: "echoes JSON".into(),
        command: "/bin/cat".into(),
        args: vec![],
        working_dir: None,
        env: HashMap::new(),
        timeout_ms: 5000,
        input_schema: None,
    };
    let tool = CommandTool::new(def);
    let input = json!({"birth": {"date": "1990-01-15"}, "question": "test"});
    let result = tool.execute(input.clone(), &ctx()).await.unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output, input);
}

// ---------------------------------------------------------------------------
// Phase 2: Registry persists and reloads
// ---------------------------------------------------------------------------

#[test]
fn registry_persist_reload_cycle() {
    let tmp = TempDir::new().unwrap();
    let mut reg = CommandToolRegistry::new(tmp.path()).unwrap();

    for i in 0..3 {
        reg.register(CommandToolDef {
            name: format!("tool_{}", i),
            description: format!("Tool {}", i),
            command: "/bin/echo".into(),
            args: vec![],
            working_dir: None,
            env: HashMap::new(),
            timeout_ms: 5000,
            input_schema: None,
        })
        .unwrap();
    }
    assert_eq!(reg.definitions().len(), 3);

    let reg2 = CommandToolRegistry::new(tmp.path()).unwrap();
    assert_eq!(reg2.definitions().len(), 3);
    let names: Vec<&str> = reg2.definitions().iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"tool_0"));
    assert!(names.contains(&"tool_1"));
    assert!(names.contains(&"tool_2"));
}

// ---------------------------------------------------------------------------
// Phase 2: Provider resolves tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_creates_working_tool() {
    let tmp = TempDir::new().unwrap();
    let mut reg = CommandToolRegistry::new(tmp.path()).unwrap();

    // Create a mock script
    let script_path = tmp.path().join("mock_tool.sh");
    {
        let mut f = std::fs::File::create(&script_path).unwrap();
        writeln!(f, "#!/bin/bash").unwrap();
        writeln!(f, "input=$(cat)").unwrap();
        writeln!(f, r#"echo '{{"tool_run_id":"mock","result":'"$input"'}}'"#).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    reg.register(CommandToolDef {
        name: "mock_bazi".into(),
        description: "Mock bazi tool".into(),
        command: script_path.to_string_lossy().into(),
        args: vec![],
        working_dir: None,
        env: HashMap::new(),
        timeout_ms: 5000,
        input_schema: None,
    })
    .unwrap();

    let provider = reg.into_provider();
    use gewu_core::traits::ToolProvider;
    let caps = provider.capabilities();
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0].name, "mock_bazi");

    // Build and execute the tool
    let assembly_ctx = gewu_core::traits::AssemblyContext {
        working_directory: tmp.path().to_path_buf(),
        provider: std::sync::Arc::new(DummyProvider),
        pool: sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap(),
        metadata_json: json!(null),
    };
    let tools = provider.build_tools("mock_bazi", &assembly_ctx).unwrap();
    assert_eq!(tools.len(), 1);

    let result = tools[0]
        .execute(json!({"test": true}), &ctx())
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output["tool_run_id"], "mock");
}

// ---------------------------------------------------------------------------
// Phase 3: register_tool persists via composer tool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn register_tool_creates_yaml_file() {
    let tmp = TempDir::new().unwrap();
    let tools_dir = tmp.path().join("tools");
    std::fs::create_dir_all(&tools_dir).unwrap();

    let tool = gewu_runtime::composer::RegisterToolTool::new(tools_dir.clone());
    let result = tool
        .execute(
            json!({
                "name": "my_cmd_tool",
                "description": "A test command tool",
                "command": "/bin/echo",
                "args": ["hello"],
                "timeout_ms": 10000
            }),
            &ctx(),
        )
        .await
        .unwrap();

    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output["ok"], json!(true));
    assert_eq!(output["name"], "my_cmd_tool");

    // Verify YAML file was created
    let yaml_path = tools_dir.join("my_cmd_tool.yaml");
    assert!(yaml_path.exists());

    // Verify it can be reloaded
    let reg = CommandToolRegistry::new(&tools_dir).unwrap();
    assert_eq!(reg.definitions().len(), 1);
    assert_eq!(reg.definitions()[0].name, "my_cmd_tool");
}

#[tokio::test]
async fn register_tool_rejects_missing_command() {
    let tmp = TempDir::new().unwrap();
    let tools_dir = tmp.path().join("tools");
    std::fs::create_dir_all(&tools_dir).unwrap();

    let tool = gewu_runtime::composer::RegisterToolTool::new(tools_dir);
    let result = tool
        .execute(
            json!({
                "name": "bad_tool",
                "description": "won't work",
                "command": "/nonexistent/binary"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output["ok"], json!(false));
    assert!(
        output["error"]
            .as_str()
            .unwrap()
            .contains("Command not found")
    );
}

// ---------------------------------------------------------------------------
// Phase 4: Full pipeline — register → reload → resolve → execute
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_pipeline_register_reload_execute() {
    let tmp = TempDir::new().unwrap();
    let tools_dir = tmp.path().join("tools");

    // Create mock script
    let script_path = tmp.path().join("mock_fw.sh");
    {
        let mut f = std::fs::File::create(&script_path).unwrap();
        writeln!(f, "#!/bin/bash").unwrap();
        writeln!(f, "input=$(cat)").unwrap();
        writeln!(
            f,
            r#"echo '{{"tool_run_id":"fw-mock","summary":"ok","data":'"$input"'}}'"#
        )
        .unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Step 1: Register via register_tool
    let reg_tool = gewu_runtime::composer::RegisterToolTool::new(tools_dir.clone());
    let r = reg_tool
        .execute(
            json!({
                "name": "mock_compute_chart",
                "description": "Mock chart computation",
                "command": script_path.to_string_lossy(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let out: serde_json::Value = serde_json::from_str(&r.output).unwrap();
    assert_eq!(out["ok"], json!(true));

    // Step 2: Reload from disk (simulates next runtime build)
    let reg = CommandToolRegistry::new(&tools_dir).unwrap();
    assert_eq!(reg.definitions().len(), 1);

    // Step 3: Convert to provider and resolve
    let provider = reg.into_provider();
    let assembly_ctx = gewu_core::traits::AssemblyContext {
        working_directory: tmp.path().to_path_buf(),
        provider: std::sync::Arc::new(DummyProvider),
        pool: sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap(),
        metadata_json: json!(null),
    };
    use gewu_core::traits::ToolProvider;
    let tools = provider
        .build_tools("mock_compute_chart", &assembly_ctx)
        .unwrap();
    assert_eq!(tools.len(), 1);

    // Step 4: Execute
    let result = tools[0]
        .execute(json!({"birth": "1990-01-15"}), &ctx())
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output["tool_run_id"], "fw-mock");
    assert_eq!(output["summary"], "ok");
}

#[tokio::test]
async fn gene_resolve_symbol_example_executes_against_real_ucf_when_configured() {
    let Ok(manifest_path) = std::env::var("GEWU_ANNO_CLI_MANIFEST") else {
        eprintln!("skipping: set GEWU_ANNO_CLI_MANIFEST to bio-anno/Cargo.toml");
        return;
    };
    let Ok(gene_info_path) = std::env::var("GEWU_GENE_INFO_UCF") else {
        eprintln!("skipping: set GEWU_GENE_INFO_UCF to gene_info.ucf");
        return;
    };
    if !PathBuf::from(&manifest_path).exists() || !PathBuf::from(&gene_info_path).exists() {
        eprintln!("skipping: configured gene resolver paths do not exist");
        return;
    }

    let yaml_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("gewu-runtime has a parent repo directory")
        .join("examples/tools/gene.resolve_symbol.yaml");
    let yaml =
        std::fs::read_to_string(&yaml_path).expect("example gene.resolve_symbol YAML exists");
    let def: CommandToolDef = serde_yaml::from_str(&yaml).expect("example YAML parses");
    let tool = CommandTool::new(def);

    let result = tool
        .execute(
            json!({
                "symbol": "SPI1",
                "assembly": "hg38",
                "species": "human",
                "locus_mode": "gene_body"
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();

    assert_eq!(output["status"], "ok");
    assert_eq!(output["query_symbol"], "SPI1");
    assert_eq!(output["assembly"], "hg38");
    assert_eq!(output["species"], "human");
    assert_eq!(output["source"], gene_info_path);
    assert_eq!(output["primary"]["region"], "chr11:47354860-47395607");
}

#[test]
fn examples_tool_directory_is_not_the_default_command_tool_registry() {
    let default_dir = CommandToolRegistry::default_dir();
    let examples_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("gewu-runtime has a parent repo directory")
        .join("examples/tools");

    assert_ne!(default_dir, examples_dir);
    assert!(examples_dir.join("gene.resolve_symbol.yaml").exists());
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct DummyProvider;

#[async_trait::async_trait]
impl gewu_core::traits::LlmProvider for DummyProvider {
    async fn stream_response<'a>(
        &'a self,
        _messages: &[gewu_core::types::Message],
        _tools: &[std::sync::Arc<dyn Tool>],
    ) -> Result<
        futures::stream::BoxStream<
            'a,
            Result<gewu_core::types::ProviderEvent, Box<dyn std::error::Error + Send + Sync>>,
        >,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        unimplemented!()
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(vec![])
    }
}
