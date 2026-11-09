use async_trait::async_trait;
use futures::stream::BoxStream;
use gewu_core::agent::AgentBuilder;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{Message, ProviderEvent};
use serde_json::Value;
use std::error::Error;
use std::sync::Arc;

struct DummyLlm;

#[async_trait]
impl LlmProvider for DummyLlm {
    fn build(
        &self,
        _messages: &[Message],
        _tools: &[Arc<dyn Tool>],
    ) -> Result<Value, Box<dyn Error + Send + Sync>> {
        Ok(Value::Null)
    }

    async fn send<'a>(
        &'a self,
        _request: Value,
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        Err("Not implemented".into())
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
}

#[test]
fn test_builder_requires_llm() {
    let result = AgentBuilder::default().with_tools(vec![]).build();

    assert!(result.is_err());
    if let Err(e) = result {
        assert_eq!(e.to_string(), "LLM provider is required");
    } else {
        panic!("Expected build to fail without LLM");
    }
}

#[test]
fn test_builder_with_llm_succeeds() {
    let result = AgentBuilder::default().llm(Arc::new(DummyLlm)).build();

    assert!(result.is_ok());
}

#[test]
fn test_builder_chain_methods() {
    let agent = AgentBuilder::default()
        .llm(Arc::new(DummyLlm))
        .with_tools(vec![])
        .system_prompt("You are a helpful assistant")
        .max_turns(Some(10))
        .context_limit(64_000)
        .build()
        .unwrap();

    assert_eq!(agent.context_limit, 64_000);
}
