//! End-to-end workflow compile + invoke tests.
//!
//! Uses a stub LLM provider (never called — only tool nodes exercise this
//! path) and an in-memory SQLite pool, plus a dummy `echo` tool factory so
//! we can assert blackboard contents after graph execution.

use async_trait::async_trait;
use futures::stream::BoxStream;
use gewu_core::traits::{AssemblyContext, LlmProvider, Tool, ToolFactory};
use gewu_core::types::{Message, ProviderEvent, ToolContext, ToolResult};
use gewu_graph::Blackboard;
use gewu_runtime::capability::ToolFactoryProvider;
use gewu_runtime::manifest::workflow::WorkflowManifest;
use gewu_runtime::{GeWuRuntime, WorkflowCompiler};
use schemars::schema::RootSchema;
use serde_json::{Value, json};
use sqlx::sqlite::SqlitePoolOptions;
use std::error::Error;
use std::sync::Arc;

struct StubLlm;

#[async_trait]
impl LlmProvider for StubLlm {
    async fn stream_response<'a>(
        &'a self,
        _messages: &[Message],
        _tools: &[Arc<dyn Tool>],
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        unimplemented!("stub llm provider should not be called in tool-only workflow tests")
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
}

/// `echo`: returns its input verbatim as the tool output (JSON-string serialized).
/// `tag`:  wraps input as `{ "tagged": <input> }`.
struct DummyFactory;

struct EchoTool;

#[async_trait]
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

struct TagTool;

#[async_trait]
impl Tool for TagTool {
    fn name(&self) -> &str {
        "tag"
    }
    fn description(&self) -> &str {
        "wraps input in a tagged object"
    }
    fn schema(&self) -> RootSchema {
        RootSchema::default()
    }
    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let wrapped = json!({ "tagged": args });
        Ok(ToolResult::new(
            "tag",
            serde_json::to_string(&wrapped).unwrap_or_default(),
        ))
    }
}

impl ToolFactory for DummyFactory {
    fn capabilities(&self) -> Vec<String> {
        vec!["echo".into(), "tag".into()]
    }
    fn create_tools(
        &self,
        capability: &str,
        _ctx: &AssemblyContext,
    ) -> Result<Vec<Box<dyn Tool>>, Box<dyn Error + Send + Sync>> {
        match capability {
            "echo" => Ok(vec![Box::new(EchoTool)]),
            "tag" => Ok(vec![Box::new(TagTool)]),
            _ => Ok(vec![]),
        }
    }
}

async fn test_runtime() -> Arc<GeWuRuntime> {
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    gewu_storage::run_migrations(&pool).await.unwrap();
    let provider: Arc<dyn LlmProvider> = Arc::new(StubLlm);

    let rt = GeWuRuntime::builder()
        .provider(provider)
        .pool(pool)
        .build()
        .unwrap();

    // Register our dummy factory into the runtime's capability registry.
    // Since the builder is opinionated (builtin+plugin paths), we re-register
    // by reaching into the registry via a fresh runtime built with an extra
    // plugin. To keep this test simple, we use the plugin hook below.
    //
    // The simplest route: rebuild the runtime with a bespoke plugin.
    let pool2 = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    gewu_storage::run_migrations(&pool2).await.unwrap();
    let provider2: Arc<dyn LlmProvider> = Arc::new(StubLlm);
    let mut reg = gewu_runtime::CapabilityRegistry::new();
    reg.register(Arc::new(ToolFactoryProvider::new(
        "plugin",
        "dummy",
        Box::new(DummyFactory),
    )));

    // Use the canonical builder via a "plugin" that exposes these factories.
    struct DummyPlugin;
    impl gewu_core::traits::Plugin for DummyPlugin {
        fn name(&self) -> &str {
            "dummy"
        }
        fn tool_factories(&self) -> Vec<Box<dyn ToolFactory>> {
            vec![Box::new(DummyFactory)]
        }
        fn specialist_descriptors(&self) -> Vec<gewu_core::types::SpecialistDescriptor> {
            vec![]
        }
    }
    let rt_with = GeWuRuntime::builder()
        .provider(provider2)
        .pool(pool2)
        .register_plugin(Box::new(DummyPlugin))
        .build()
        .unwrap();
    drop(rt);
    Arc::new(rt_with)
}

#[tokio::test]
async fn compile_and_invoke_linear_tool_workflow() {
    let rt = test_runtime().await;
    let yaml = r#"
id: linear
entry: a
finish: [b]
nodes:
  a: { kind: tool, tool: echo, write: first }
  b: { kind: tool, tool: tag, input: $.first, write: second }
edges:
  - [a, b]
"#;
    let m: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
    let compiler = WorkflowCompiler::new(rt.clone());
    let graph = compiler.compile(&m).unwrap();

    let input = Blackboard::from_value(json!({ "hello": "world" }));
    let out = graph.ainvoke(input).await.unwrap();
    // echo returned the full input blackboard (since input: None), which was the initial Value
    assert!(
        out.get_path("first").is_some(),
        "first missing: {:?}",
        out.0
    );
    assert!(
        out.get_path("second.tagged").is_some(),
        "second.tagged missing: {:?}",
        out.0
    );
}

#[tokio::test]
async fn compile_and_invoke_parallel_tool_workflow() {
    let rt = test_runtime().await;
    let yaml = r#"
id: par
entry: split
finish: [split]
nodes:
  split:
    kind: parallel
    branches:
      left:  { kind: tool, tool: echo, write: runs.left }
      right: { kind: tool, tool: tag,  write: runs.right }
edges: []
"#;
    let m: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
    let compiler = WorkflowCompiler::new(rt.clone());
    let graph = compiler.compile(&m).unwrap();

    let input = Blackboard::from_value(json!({ "k": 7 }));
    let out = graph.ainvoke(input).await.unwrap();
    assert!(
        out.get_path("runs.left").is_some(),
        "left missing: {:?}",
        out.0
    );
    assert!(
        out.get_path("runs.right.tagged").is_some(),
        "right.tagged missing: {:?}",
        out.0
    );
}

#[tokio::test]
async fn conditional_edge_routes_to_correct_branch() {
    let rt = test_runtime().await;
    // Branch A if input.route == "x", else B. Both write a distinct marker.
    let yaml = r#"
id: branchy
entry: decide
finish: [left, right]
nodes:
  decide: { kind: tool, tool: echo, write: decision }
  left:   { kind: tool, tool: tag,  write: went_left }
  right:  { kind: tool, tool: tag,  write: went_right }
edges:
  - { from: decide, to: left,  when: '$.decision.route == "x"' }
  - { from: decide, to: right, when: '$.decision.route != "x"' }
"#;
    let m: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
    let compiler = WorkflowCompiler::new(rt.clone());
    let graph = compiler.compile(&m).unwrap();

    // route = "x" → left
    let out_left = graph
        .ainvoke(Blackboard::from_value(json!({ "route": "x" })))
        .await
        .unwrap();
    assert!(
        out_left.get_path("went_left").is_some(),
        "expected went_left; got {:?}",
        out_left.0
    );
    assert!(out_left.get_path("went_right").is_none());

    // route = "y" → right
    let out_right = graph
        .ainvoke(Blackboard::from_value(json!({ "route": "y" })))
        .await
        .unwrap();
    assert!(
        out_right.get_path("went_right").is_some(),
        "expected went_right; got {:?}",
        out_right.0
    );
    assert!(out_right.get_path("went_left").is_none());
}

#[tokio::test]
async fn validate_rejects_unknown_capability() {
    let rt = test_runtime().await;
    let yaml = r#"
id: bad
entry: a
finish: [a]
nodes:
  a: { kind: tool, tool: does_not_exist }
edges: []
"#;
    let m: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
    let compiler = WorkflowCompiler::new(rt.clone());
    let err = compiler.compile(&m).unwrap_err().to_string();
    assert!(err.contains("unknown capability"), "got: {err}");
}
