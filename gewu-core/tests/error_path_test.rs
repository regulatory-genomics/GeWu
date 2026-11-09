use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use gewu_core::Agent;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{AgentEvent, Message, ProviderEvent, ToolContext, ToolResult};
use serde_json::Value;
use std::error::Error;
use std::sync::Arc;

struct MockLlmProvider {
    tool_behavior: &'static str,
}

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
        Ok(serde_json::json!({
            "tool_response_count": tool_response_count,
            "behavior": self.tool_behavior,
        }))
    }

    async fn send<'a>(
        &'a self,
        request: Value,
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        let tool_response_count = request["tool_response_count"].as_u64().unwrap_or(0) as usize;
        let behavior = request["behavior"].as_str().unwrap_or("").to_string();

        let s = async_stream::try_stream! {
            match behavior.as_str() {
                "not_found" => {
                    yield ProviderEvent::ToolCallStart { id: "call_1".to_string(), name: "nonexistent_tool".to_string() };
                    yield ProviderEvent::ToolCallDelta { id: "call_1".to_string(), delta: "{}".to_string() };
                    yield ProviderEvent::Finish { reason: Some("tool_calls".to_string()) };
                }
                "empty_name" => {
                    yield ProviderEvent::ToolCallStart { id: "call_1".to_string(), name: "".to_string() };
                    yield ProviderEvent::ToolCallDelta { id: "call_1".to_string(), delta: "{}".to_string() };
                    yield ProviderEvent::Finish { reason: Some("tool_calls".to_string()) };
                }
                _ => {
                    if tool_response_count >= 3 {
                        yield ProviderEvent::ContentDelta { delta: "Done".to_string() };
                        yield ProviderEvent::Finish { reason: Some("stop".to_string()) };
                    } else {
                        let call_id = format!("call_{}", tool_response_count + 1);
                        yield ProviderEvent::ToolCallStart { id: call_id.clone(), name: "test_tool".to_string() };
                        yield ProviderEvent::ToolCallDelta { id: call_id, delta: "{}".to_string() };
                        yield ProviderEvent::Finish { reason: Some("tool_calls".to_string()) };
                    }
                }
            }
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
async fn test_tool_not_found_error() {
    let llm = MockLlmProvider {
        tool_behavior: "not_found",
    };

    let agent = Agent::builder()
        .llm(Arc::new(llm))
        .with_tools(vec![Arc::new(TestTool) as Arc<dyn Tool>])
        .build()
        .unwrap();

    let mut agent = agent;
    let mut stream = Box::pin(agent.ask("Test"));
    let mut tool_found_error = false;

    while let Some(event_result) = stream.next().await {
        if let Ok(AgentEvent::ToolFinish { output, .. }) = event_result {
            if output.contains("Tool not found:") {
                tool_found_error = true;
                break;
            }
        }
    }

    assert!(
        tool_found_error,
        "Should have received tool not found error in ToolFinish"
    );
}

#[tokio::test]
async fn test_agent_reset_clears_memory() {
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use gewu_core::traits::LlmProvider;
    use gewu_core::types::{Message, ProviderEvent};
    use std::error::Error;

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

    let mut agent = Agent::builder()
        .llm(Arc::new(DummyLlm))
        .system_prompt("You are a helpful assistant")
        .build()
        .unwrap();

    agent
        .memory
        .add_message(Message::User { content: vec![] })
        .await;
    assert_eq!(agent.memory.messages().len(), 1);

    agent.reset().await;
    assert_eq!(agent.memory.messages().len(), 1);
}

#[tokio::test]
async fn test_empty_tool_name_skipped() {
    let llm = MockLlmProvider {
        tool_behavior: "empty_name",
    };

    let agent = Agent::builder()
        .llm(Arc::new(llm))
        .with_tools(vec![Arc::new(TestTool) as Arc<dyn Tool>])
        .build()
        .unwrap();

    let mut agent = agent;
    let mut stream = Box::pin(agent.ask("Test"));
    let mut errors = Vec::new();

    while let Some(event_result) = stream.next().await {
        if let Ok(AgentEvent::Error { error }) = event_result {
            errors.push(error);
        }
    }

    assert!(!errors.is_empty());
    assert!(errors.iter().any(|e| e.contains("Tool name is empty")));
}

#[tokio::test]
async fn test_max_turns_reached() {
    let llm = MockLlmProvider {
        tool_behavior: "default",
    };

    let agent = Agent::builder()
        .llm(Arc::new(llm))
        .with_tools(vec![Arc::new(TestTool) as Arc<dyn Tool>])
        .max_turns(Some(2))
        .build()
        .unwrap();

    let mut agent = agent;
    let mut stream = Box::pin(agent.ask("Test"));
    let mut error_found = false;

    while let Some(event_result) = stream.next().await {
        if let Ok(AgentEvent::Error { error }) = event_result {
            assert!(error.contains("Max turns"));
            error_found = true;
            break;
        }
    }

    assert!(error_found, "Should have received max turns error");
}

#[tokio::test]
async fn test_agent_reset_restores_system_prompt() {
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use gewu_core::traits::LlmProvider;
    use std::error::Error;

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

    let mut agent = Agent::builder()
        .llm(Arc::new(DummyLlm))
        .system_prompt("You are a helpful assistant")
        .build()
        .unwrap();

    agent.reset().await;

    assert_eq!(agent.memory.messages().len(), 1);
    if let Message::System { content } = &agent.memory.messages()[0] {
        assert_eq!(content, "You are a helpful assistant");
    } else {
        panic!("Expected system message after reset");
    }
}
