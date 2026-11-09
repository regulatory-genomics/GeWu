use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use gewu_core::Agent;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{AgentEvent, Message, ProviderEvent, ToolContext, ToolResult};
use serde_json::Value;
use std::error::Error;
use std::sync::Arc;

struct MockLlmProvider;

#[async_trait]
impl LlmProvider for MockLlmProvider {
    fn build(
        &self,
        messages: &[Message],
        _tools: &[Arc<dyn Tool>],
    ) -> Result<Value, Box<dyn Error + Send + Sync>> {
        let tool_response_count = messages
            .iter()
            .filter(|m| matches!(m, Message::Tool { .. }))
            .count();
        Ok(serde_json::json!({ "tool_response_count": tool_response_count }))
    }

    async fn send<'a>(
        &'a self,
        request: Value,
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        let tool_response_count = request["tool_response_count"].as_u64().unwrap_or(0) as usize;

        if tool_response_count >= 5 {
            let s = async_stream::try_stream! {
                yield ProviderEvent::ContentDelta { delta: "Done".to_string() };
                yield ProviderEvent::Finish { reason: Some("stop".to_string()) };
            };
            return Ok(Box::pin(s));
        }

        let call_id = format!("call_{}", tool_response_count);
        let s = async_stream::try_stream! {
            yield ProviderEvent::ToolCallStart { id: call_id.clone(), name: "test_tool".to_string() };
            yield ProviderEvent::ToolCallDelta { id: call_id, delta: "{}".to_string() };
            yield ProviderEvent::Finish { reason: Some("tool_calls".to_string()) };
        };
        Ok(Box::pin(s))
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec!["mock".to_string()])
    }
}

struct TestTool;

#[async_trait]
impl Tool for TestTool {
    fn name(&self) -> &str {
        "test_tool"
    }
    fn description(&self) -> &str {
        "test"
    }
    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(serde_json::Value)
    }
    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        Ok(ToolResult::new("test_tool", "output".to_string()))
    }
}

#[tokio::test]
async fn test_doom_loop_detection() {
    let agent = Agent::builder()
        .llm(Arc::new(MockLlmProvider))
        .with_tools(vec![Arc::new(TestTool) as Arc<dyn Tool>])
        .build()
        .unwrap();

    let mut agent = agent;
    let mut stream = Box::pin(agent.ask("Trigger doom loop"));
    let mut response_text = String::new();
    let mut error_text = String::new();

    while let Some(event_result) = stream.next().await {
        match event_result {
            Ok(event) => match event {
                AgentEvent::ContentDelta { delta } => response_text.push_str(&delta),
                AgentEvent::Finish { response } => response_text = response,
                AgentEvent::Error { error } => error_text = error,
                _ => {}
            },
            Err(e) => panic!("Stream error: {}", e),
        }
    }

    assert!(
        error_text.contains("Doom loop detected"),
        "Error was: {}",
        error_text
    );
    assert!(error_text.contains("Stopping"), "Error was: {}", error_text);
}
