use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use gewu_core::agent::AgentBuilder;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{
    AgentEvent, FunctionCall, Message, ProviderEvent, ToolCall, ToolContext, ToolExecutionPolicy,
    ToolResult,
};
use serde_json::Value;
use std::error::Error;
use std::sync::{Arc, Mutex};

struct MockLlm {
    responses: Arc<Mutex<Vec<Message>>>,
}

#[derive(Debug)]
struct MockError(String);
impl std::fmt::Display for MockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl Error for MockError {}

#[async_trait]
impl LlmProvider for MockLlm {
    async fn stream_response<'a>(
        &'a self,
        _messages: &[Message],
        _tools: &[Arc<dyn Tool>],
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            return Err(Box::new(MockError("No more mock responses".to_string())));
        }
        let response = responses.remove(0);

        let s = async_stream::try_stream! {
            match response {
                Message::Assistant { content, tool_calls } => {
                    if let Some(c) = content {
                        yield ProviderEvent::ContentDelta { delta: c };
                    }
                    if let Some(calls) = tool_calls {
                        for call in calls {
                            yield ProviderEvent::ToolCallStart { id: call.id.clone(), name: call.function.name };
                            yield ProviderEvent::ToolCallDelta { id: call.id, delta: call.function.arguments };
                        }
                    }
                }
                _ => {}
            }
        };
        Ok(Box::pin(s))
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec!["mock".to_string()])
    }
}

struct MockTool;

#[async_trait]
impl Tool for MockTool {
    fn name(&self) -> &str {
        "mock_tool"
    }
    fn description(&self) -> &str {
        "Mock tool"
    }
    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(serde_json::Value)
    }
    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        Ok(ToolResult::new("mock_tool", "Tool output".to_string()))
    }
}

struct ParallelTaskTool;

#[async_trait]
impl Tool for ParallelTaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "Parallel-safe task tool"
    }

    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(serde_json::Value)
    }

    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::ParallelSafe
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let label = args
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        Ok(ToolResult::new("task", format!("done-{label}")))
    }
}

#[tokio::test]
async fn test_agent_loop() {
    let tool_call = ToolCall {
        id: "call_1".to_string(),
        r#type: "function".to_string(),
        function: FunctionCall {
            name: "mock_tool".to_string(),
            arguments: "{}".to_string(),
        },
    };

    let responses = vec![
        Message::Assistant {
            content: None,
            tool_calls: Some(vec![tool_call]),
        },
        Message::Assistant {
            content: Some("Done: Tool output".to_string()),
            tool_calls: None,
        },
    ];

    let llm = Arc::new(MockLlm {
        responses: Arc::new(Mutex::new(responses)),
    });

    let tool: Arc<dyn Tool> = Arc::new(MockTool);

    let mut agent = AgentBuilder::default()
        .llm(llm)
        .with_tools(vec![tool])
        .system_prompt("System")
        .build()
        .unwrap();

    let mut stream = Box::pin(agent.ask("Do something"));
    let mut result = String::new();
    while let Some(event) = stream.next().await {
        if let Ok(AgentEvent::ContentDelta { delta }) = event {
            result.push_str(&delta);
        } else if let Ok(AgentEvent::Finish { response }) = event {
            result = response;
        }
    }

    assert_eq!(result, "Done: Tool output");
}

#[tokio::test]
async fn parallel_task_wave_emits_all_starts_before_finishes() {
    let task_call_a = ToolCall {
        id: "task_a".to_string(),
        r#type: "function".to_string(),
        function: FunctionCall {
            name: "task".to_string(),
            arguments: r#"{"label":"a"}"#.to_string(),
        },
    };
    let task_call_b = ToolCall {
        id: "task_b".to_string(),
        r#type: "function".to_string(),
        function: FunctionCall {
            name: "task".to_string(),
            arguments: r#"{"label":"b"}"#.to_string(),
        },
    };

    let responses = vec![
        Message::Assistant {
            content: None,
            tool_calls: Some(vec![task_call_a, task_call_b]),
        },
        Message::Assistant {
            content: Some("Both tasks are done".to_string()),
            tool_calls: None,
        },
    ];

    let llm = Arc::new(MockLlm {
        responses: Arc::new(Mutex::new(responses)),
    });
    let tool: Arc<dyn Tool> = Arc::new(ParallelTaskTool);
    let mut agent = AgentBuilder::default()
        .llm(llm)
        .with_tools(vec![tool])
        .system_prompt("System")
        .build()
        .unwrap();

    let mut stream = Box::pin(agent.ask("Run two tasks"));
    let mut observed = Vec::new();
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            AgentEvent::ToolStart { tool_call_id, .. } => {
                observed.push(format!("start:{tool_call_id}"));
            }
            AgentEvent::ToolFinish { tool_call_id, .. } => {
                observed.push(format!("finish:{tool_call_id}"));
            }
            AgentEvent::Finish { .. } => break,
            _ => {}
        }
    }

    assert_eq!(
        observed[..4],
        [
            "start:task_a".to_string(),
            "start:task_b".to_string(),
            "finish:task_a".to_string(),
            "finish:task_b".to_string(),
        ]
    );
}
