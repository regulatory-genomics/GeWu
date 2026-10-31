use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use gewu_core::agent::AgentBuilder;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{
    AgentEvent, FunctionCall, Message, ProviderEvent, ToolCall, ToolContext, ToolResult,
};
use gewu_tools::ApplyPatchTool;
use serde_json::Value;
use std::error::Error;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

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
    fn build(
        &self,
        _messages: &[Message],
        _tools: &[Arc<dyn Tool>],
    ) -> Result<Value, Box<dyn Error + Send + Sync>> {
        Ok(serde_json::json!({ "model": "mock" }))
    }

    async fn send<'a>(
        &'a self,
        _request: Value,
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
        let mut result = ToolResult::new("Mock Result Title", "Tool output".to_string());
        result
            .metadata
            .insert("kind".to_string(), serde_json::json!("mock"));
        Ok(result)
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
    let mut tool_finish = None;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            AgentEvent::ToolFinish {
                call_id,
                tool,
                title,
                output,
                metadata,
            } => tool_finish = Some((call_id, tool, title, output, metadata)),
            AgentEvent::ContentDelta { delta } => result.push_str(&delta),
            AgentEvent::Finish { response } => result = response,
            _ => {}
        }
    }

    let (call_id, tool, title, output, metadata) = tool_finish.unwrap();
    assert_eq!(call_id, "call_1");
    assert_eq!(tool, "mock_tool");
    assert_eq!(title, "Mock Result Title");
    assert_eq!(output, "Tool output");
    assert_eq!(metadata["kind"], serde_json::json!("mock"));
    assert_eq!(result, "Done: Tool output");
}

#[tokio::test]
async fn test_apply_patch_metadata_through_agent_loop() {
    let directory = tempdir().unwrap();
    let tool_call = ToolCall {
        id: "call_patch".to_string(),
        r#type: "function".to_string(),
        function: FunctionCall {
            name: "apply_patch".to_string(),
            arguments: serde_json::json!({
                "patchText": "*** Begin Patch\n*** Add File: created.txt\n+created\n*** End Patch"
            })
            .to_string(),
        },
    };
    let llm = Arc::new(MockLlm {
        responses: Arc::new(Mutex::new(vec![
            Message::Assistant {
                content: None,
                tool_calls: Some(vec![tool_call]),
            },
            Message::Assistant {
                content: Some("patched".to_string()),
                tool_calls: None,
            },
        ])),
    });
    let tool: Arc<dyn Tool> = Arc::new(ApplyPatchTool {
        working_directory: directory.path().to_path_buf(),
    });
    let mut agent = AgentBuilder::default()
        .llm(llm)
        .with_tools(vec![tool])
        .system_prompt("System")
        .build()
        .unwrap();
    let mut stream = Box::pin(agent.ask("Create a file"));
    let mut finish = None;

    while let Some(event) = stream.next().await {
        if let AgentEvent::ToolFinish {
            call_id,
            tool,
            title,
            output,
            metadata,
        } = event.unwrap()
        {
            finish = Some((call_id, tool, title, output, metadata));
        }
    }

    let (_call_id, tool, title, output, metadata) = finish.expect("missing ToolFinish");
    assert_eq!(tool, "apply_patch");
    assert_eq!(title, "apply_patch");
    assert!(output.contains("A created.txt"));
    assert_eq!(metadata["files"][0]["operation"], "add");
    assert_eq!(
        tokio::fs::read_to_string(directory.path().join("created.txt"))
            .await
            .unwrap(),
        "created\n"
    );
}
