//! Specialist assembly: the allowlist is exact and nothing is added implicitly.

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{Message, ProviderEvent, ToolContext, ToolResult};
use gewu_runtime::{SpecialistBuild, SpecialistDescriptor, SpecialistError, SpecialistRegistry};
use serde_json::Value;
use std::error::Error;
use std::sync::{Arc, Mutex};

struct NamedTool(&'static str);

#[async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "test tool"
    }
    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(Value)
    }
    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        Ok(ToolResult::new(self.0, "ok"))
    }
}

/// Records the tool names offered to the model, then answers without tools.
struct RecordingLlm {
    offered: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait]
impl LlmProvider for RecordingLlm {
    fn build(
        &self,
        _messages: &[Message],
        tools: &[Arc<dyn Tool>],
    ) -> Result<Value, Box<dyn Error + Send + Sync>> {
        self.offered
            .lock()
            .unwrap()
            .push(tools.iter().map(|tool| tool.name().to_string()).collect());
        Ok(Value::Null)
    }

    async fn send<'a>(
        &'a self,
        _request: Value,
    ) -> Result<BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>, Box<dyn Error + Send + Sync>>
    {
        let s = async_stream::try_stream! {
            yield ProviderEvent::ContentDelta { delta: "ok".to_string() };
            yield ProviderEvent::Finish { reason: None };
        };
        Ok(Box::pin(s))
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(Vec::new())
    }
}

fn descriptor(tools: &[&str]) -> SpecialistDescriptor {
    SpecialistDescriptor {
        id: "browser".into(),
        description: "Tachyon browser assistant".into(),
        prompt: "Drive the browser.".into(),
        tools: tools.iter().map(|name| name.to_string()).collect(),
        max_turns: Some(4),
        max_tool_calls: Some(8),
    }
}

#[tokio::test]
async fn the_agent_only_offers_the_declared_tools() {
    let mut registry = SpecialistRegistry::new();
    registry
        .register_all(vec![
            Arc::new(NamedTool("locus_set")) as Arc<dyn Tool>,
            Arc::new(NamedTool("track_add")) as Arc<dyn Tool>,
            Arc::new(NamedTool("bash")) as Arc<dyn Tool>,
        ])
        .expect("register");

    let offered = Arc::new(Mutex::new(Vec::new()));
    let llm = Arc::new(RecordingLlm {
        offered: offered.clone(),
    });
    let mut agent = registry
        .build(
            &descriptor(&["locus_set", "track_add"]),
            SpecialistBuild { llm, memory: None },
        )
        .expect("build");

    let mut stream = Box::pin(agent.ask("go"));
    while stream.next().await.is_some() {}

    let offered = offered.lock().unwrap();
    assert_eq!(offered.len(), 1);
    assert_eq!(
        offered[0],
        vec!["locus_set".to_string(), "track_add".to_string()],
        "registered-but-unlisted tools must not reach the model"
    );
}

#[tokio::test]
async fn a_duplicate_tool_registration_is_rejected() {
    let mut registry = SpecialistRegistry::new();
    registry
        .register(Arc::new(NamedTool("locus_set")) as Arc<dyn Tool>)
        .expect("first");

    let error = registry
        .register(Arc::new(NamedTool("locus_set")) as Arc<dyn Tool>)
        .err()
        .expect("duplicate must fail");
    assert!(matches!(error, SpecialistError::DuplicateTool { .. }));
    assert_eq!(registry.len(), 1);
}

#[tokio::test]
async fn a_tool_listed_twice_is_rejected() {
    let mut registry = SpecialistRegistry::new();
    registry
        .register(Arc::new(NamedTool("locus_set")) as Arc<dyn Tool>)
        .expect("register");

    let error = registry
        .resolve(&descriptor(&["locus_set", "locus_set"]))
        .err()
        .expect("repeated tool must fail");
    assert!(matches!(error, SpecialistError::RepeatedTool { .. }));
}

#[tokio::test]
async fn an_empty_tool_list_builds_a_tool_free_agent() {
    let registry = SpecialistRegistry::new();
    let offered = Arc::new(Mutex::new(Vec::new()));
    let mut agent = registry
        .build(
            &descriptor(&[]),
            SpecialistBuild {
                llm: Arc::new(RecordingLlm {
                    offered: offered.clone(),
                }),
                memory: None,
            },
        )
        .expect("build");

    let mut stream = Box::pin(agent.ask("go"));
    while stream.next().await.is_some() {}

    assert!(offered.lock().unwrap()[0].is_empty());
}

#[tokio::test]
async fn registry_names_are_stable_and_sorted() {
    let mut registry = SpecialistRegistry::new();
    registry
        .register_all(vec![
            Arc::new(NamedTool("track_add")) as Arc<dyn Tool>,
            Arc::new(NamedTool("locus_set")) as Arc<dyn Tool>,
        ])
        .expect("register");

    assert_eq!(
        registry.names(),
        vec!["locus_set".to_string(), "track_add".to_string()]
    );
    assert!(registry.contains("locus_set"));
    assert!(!registry.contains("bash"));
}
