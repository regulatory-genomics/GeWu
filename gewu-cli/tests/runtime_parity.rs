use async_trait::async_trait;
use futures::stream::BoxStream;
use gewu_cli::create_agent;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{Message, ProviderEvent, ToolContext, ToolResult};
use gewu_runtime::GeWuRuntime as RuntimeGeWuRuntime;
use schemars::schema::RootSchema;
use serde_json::Value;
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
        unimplemented!("stub")
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
}

struct ExtraTool;

#[async_trait]
impl Tool for ExtraTool {
    fn name(&self) -> &str {
        "extra_tool"
    }

    fn description(&self) -> &str {
        "Extra test tool"
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(Value)
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        Ok(ToolResult::new("extra_tool", "ok"))
    }
}

async fn tool_names_from_wrapper(
    agent_name: &str,
    additional_tools: Vec<Arc<dyn Tool>>,
) -> Vec<String> {
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let provider: Arc<dyn LlmProvider> = Arc::new(StubLlm);
    let (_, registry) = create_agent(
        agent_name,
        provider,
        pool,
        format!("wrapper-{}", agent_name),
        additional_tools,
    )
    .await
    .unwrap();
    registry.list_tools()
}

async fn tool_names_from_runtime(
    agent_name: &str,
    additional_tools: Vec<Arc<dyn Tool>>,
) -> Vec<String> {
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let provider: Arc<dyn LlmProvider> = Arc::new(StubLlm);
    let runtime = RuntimeGeWuRuntime::builder()
        .provider(provider)
        .pool(pool)
        .register_builtin_capabilities()
        .register_builtin_specialists()
        .build()
        .unwrap();
    let (_, registry) = runtime
        .build_specialist(
            agent_name,
            format!("runtime-{}", agent_name),
            additional_tools,
        )
        .await
        .unwrap();
    registry.list_tools()
}

#[tokio::test]
async fn test_wrapper_matches_runtime_for_build() {
    let wrapper = tool_names_from_wrapper("build", vec![]).await;
    let runtime = tool_names_from_runtime("build", vec![]).await;
    assert_eq!(wrapper, runtime);
    assert!(wrapper.contains(&"task".to_string()));
    assert!(wrapper.contains(&"bash".to_string()));
}

#[tokio::test]
async fn test_wrapper_matches_runtime_for_explore() {
    let wrapper = tool_names_from_wrapper("explore", vec![]).await;
    let runtime = tool_names_from_runtime("explore", vec![]).await;
    assert_eq!(wrapper, runtime);
    assert!(!wrapper.contains(&"task".to_string()));
    assert!(wrapper.contains(&"bash".to_string()));
}

#[tokio::test]
async fn test_wrapper_matches_runtime_for_general() {
    let wrapper = tool_names_from_wrapper("general", vec![]).await;
    let runtime = tool_names_from_runtime("general", vec![]).await;
    assert_eq!(wrapper, runtime);
    assert!(wrapper.contains(&"task".to_string()));
}

#[tokio::test]
async fn test_additional_tools_and_batch_tool_match() {
    let extra: Arc<dyn Tool> = Arc::new(ExtraTool);

    let wrapper = tool_names_from_wrapper("explore", vec![extra.clone()]).await;
    let runtime = tool_names_from_runtime("explore", vec![extra]).await;

    assert_eq!(wrapper, runtime);
    assert!(wrapper.contains(&"extra_tool".to_string()));
}
