use async_trait::async_trait;
use futures::stream::BoxStream;
use gewu_core::traits::{AssemblyContext, LlmProvider, Plugin, Tool, ToolFactory};
use gewu_core::types::{Message, ProviderEvent, SpecialistDescriptor, ToolContext, ToolResult};
use gewu_runtime::GeWuRuntime;
use schemars::schema::RootSchema;
use sqlx::sqlite::SqlitePoolOptions;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

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
        unimplemented!("stub")
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
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

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(serde_json::Value)
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let _ = &self.working_directory;
        Ok(ToolResult::new("count_lines", "0"))
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
                "glob".into(),
                "count_lines".into(),
                "report_result".into(),
            ],
            skills: Vec::new(),
            system_prompt: "You count lines in files.".into(),
        }]
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_plugin_specialist_assembly() {
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let provider: Arc<dyn LlmProvider> = Arc::new(StubLlm);

    let runtime = GeWuRuntime::builder()
        .provider(provider)
        .pool(pool)
        .register_builtin_capabilities()
        .register_builtin_specialists()
        .register_plugin(Box::new(ExamplePlugin))
        .build()
        .unwrap();

    let listed = runtime.list_specialists();
    assert!(listed.iter().any(|a| a.name == "line-counter"));
    assert!(listed.iter().any(|a| a.name == "explore"));

    let specialist = runtime.find_specialist("line-counter").unwrap();
    let tools = runtime
        .resolve_capabilities(&specialist.capabilities)
        .unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();

    assert!(
        names.contains(&"count_lines"),
        "missing custom tool, got: {names:?}"
    );
    assert!(
        names.contains(&"read_file"),
        "missing builtin read_file, got: {names:?}"
    );
    assert!(
        names.contains(&"glob"),
        "missing builtin glob, got: {names:?}"
    );
    assert!(
        names.contains(&"report_result"),
        "missing builtin report_result, got: {names:?}"
    );
}

#[tokio::test]
async fn test_plugin_host_loads_from_config() {
    use gewu_runtime::{PluginHost, PluginRegistry};
    use std::collections::HashMap;

    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let provider: Arc<dyn LlmProvider> = Arc::new(StubLlm);

    let mut registry = PluginRegistry::new();
    registry.register("example", |_ctx| {
        Ok(Box::new(ExamplePlugin) as Box<dyn Plugin>)
    });

    let mut configs = HashMap::new();
    configs.insert("example".to_string(), serde_json::json!({"enabled": true}));

    let host = PluginHost::from_config(&configs, &registry, provider, pool, PathBuf::from("/tmp"))
        .unwrap();

    assert_eq!(host.plugin_names(), vec!["example"]);
    assert_eq!(host.plugins().len(), 1);
}

#[tokio::test]
async fn test_plugin_host_skips_disabled() {
    use gewu_runtime::{PluginHost, PluginRegistry};
    use std::collections::HashMap;

    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let provider: Arc<dyn LlmProvider> = Arc::new(StubLlm);

    let mut registry = PluginRegistry::new();
    registry.register("example", |_ctx| {
        Ok(Box::new(ExamplePlugin) as Box<dyn Plugin>)
    });

    let mut configs = HashMap::new();
    configs.insert("example".to_string(), serde_json::json!({"enabled": false}));

    let host = PluginHost::from_config(&configs, &registry, provider, pool, PathBuf::from("/tmp"))
        .unwrap();

    assert!(host.plugins().is_empty());
}

#[tokio::test]
async fn test_runtime_factory_builds_with_plugins() {
    use gewu_runtime::{PluginHost, PluginRegistry, RuntimeFactory};
    use std::collections::HashMap;

    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let provider: Arc<dyn LlmProvider> = Arc::new(StubLlm);

    let mut registry = PluginRegistry::new();
    registry.register("example", |_ctx| {
        Ok(Box::new(ExamplePlugin) as Box<dyn Plugin>)
    });

    let mut configs = HashMap::new();
    configs.insert("example".to_string(), serde_json::json!({"enabled": true}));

    let host = PluginHost::from_config(
        &configs,
        &registry,
        provider.clone(),
        pool.clone(),
        PathBuf::from("/tmp"),
    )
    .unwrap();

    let runtime = RuntimeFactory::build(provider, pool, &host).unwrap();

    // Should have both builtin and plugin specialists
    let listed = runtime.list_specialists();
    assert!(
        listed.iter().any(|a| a.name == "build"),
        "missing builtin build"
    );
    assert!(
        listed.iter().any(|a| a.name == "line-counter"),
        "missing plugin line-counter"
    );
}
