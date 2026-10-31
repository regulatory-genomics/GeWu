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
            if let Message::Assistant { content, reasoning, tool_calls, replay } = response {
                if let Some(replay) = replay {
                    yield ProviderEvent::ReasoningReplay { replay };
                }
                if let Some(delta) = reasoning {
                    yield ProviderEvent::ReasoningDelta { delta };
                }
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
    let replay = gewu_core::types::ReasoningReplay {
        origin: "chat:https://example.com/v1".into(),
        model: "reasoner".into(),
        field: Some("reasoning_content".into()),
        blocks: vec![],
    };
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
            reasoning: Some("I should call the tool".into()),
            replay: Some(replay.clone()),
            tool_calls: Some(vec![tool_call]),
        },
        Message::Assistant {
            content: Some("Done: Tool output".to_string()),
            reasoning: None,
            replay: None,
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
    drop(stream);
    assert!(
        agent
            .memory
            .messages()
            .iter()
            .any(|message| matches!(message,
                Message::Assistant { reasoning: Some(text), replay: Some(state), .. }
                if text == "I should call the tool" && state == &replay
            ))
    );
}

#[tokio::test]
async fn test_agent_emits_usage_at_turn_boundaries() {
    let responses = vec![Message::Assistant {
        content: Some("Done".to_string()),
        reasoning: None,
        replay: None,
        tool_calls: None,
    }];

    let llm = Arc::new(MockLlm {
        responses: Arc::new(Mutex::new(responses)),
    });

    let mut agent = AgentBuilder::default()
        .llm(llm)
        .with_tools(vec![])
        .system_prompt("System")
        .build()
        .unwrap();

    let mut stream = Box::pin(agent.ask("Do something"));
    let mut kinds = Vec::new();
    let mut usage = None;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            AgentEvent::Usage {
                used_tokens,
                context_limit,
                ..
            } => {
                kinds.push("usage");
                usage = Some((used_tokens, context_limit));
            }
            AgentEvent::Finish { .. } => kinds.push("finish"),
            _ => {}
        }
    }

    // One usage snapshot per turn, emitted before the terminal event.
    assert_eq!(kinds, vec!["usage", "finish"]);
    let (used_tokens, context_limit) = usage.unwrap();
    assert!(used_tokens > 0, "history should count: {used_tokens}");
    assert_eq!(context_limit, 256_000);
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
                reasoning: None,
                replay: None,
                tool_calls: Some(vec![tool_call]),
            },
            Message::Assistant {
                content: Some("patched".to_string()),
                reasoning: None,
                replay: None,
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

/// A tool that sleeps for a configurable duration and echoes its id, used to
/// observe parallel execution and result ordering.
struct SlowTool;

#[async_trait]
impl Tool for SlowTool {
    fn name(&self) -> &str {
        "slow_tool"
    }
    fn description(&self) -> &str {
        "Sleeps then echoes its id"
    }
    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(serde_json::Value)
    }
    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let ms = args["ms"].as_u64().unwrap_or(0);
        let id = args["id"].as_u64().unwrap_or(0);
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        Ok(ToolResult::new("slow", format!("result-{id}")))
    }
}

#[tokio::test]
async fn test_parallel_tool_calls_run_concurrently_in_call_order() {
    let make_call = |id: u64, call_id: &str| ToolCall {
        id: call_id.to_string(),
        r#type: "function".to_string(),
        function: FunctionCall {
            name: "slow_tool".to_string(),
            arguments: serde_json::json!({ "id": id, "ms": 300 }).to_string(),
        },
    };

    let responses = vec![
        Message::Assistant {
            content: None,
            reasoning: None,
            replay: None,
            tool_calls: Some(vec![
                make_call(1, "call_1"),
                make_call(2, "call_2"),
                make_call(3, "call_3"),
            ]),
        },
        Message::Assistant {
            content: Some("all done".to_string()),
            reasoning: None,
            replay: None,
            tool_calls: None,
        },
    ];

    let llm = Arc::new(MockLlm {
        responses: Arc::new(Mutex::new(responses)),
    });
    let tool: Arc<dyn Tool> = Arc::new(SlowTool);

    let mut agent = AgentBuilder::default()
        .llm(llm)
        .with_tools(vec![tool])
        .system_prompt("System")
        .build()
        .unwrap();

    let started = std::time::Instant::now();
    let mut finish_order = Vec::new();
    {
        let mut stream = Box::pin(agent.ask("Run three slow tools"));
        while let Some(event) = stream.next().await {
            if let AgentEvent::ToolFinish { output, .. } = event.unwrap() {
                finish_order.push(output);
            }
        }
    }
    let elapsed = started.elapsed();

    // Three 300ms sleeps must overlap: sequential execution would take
    // ~900ms, concurrent execution should finish well under that.
    assert!(
        elapsed < std::time::Duration::from_millis(700),
        "tool calls appear to run sequentially: took {elapsed:?}"
    );

    // Results are applied in the original call order regardless of which
    // finished first (all sleeps are equal here, so completion order is
    // nondeterministic while application order must not be).
    assert_eq!(finish_order, vec!["result-1", "result-2", "result-3"]);

    // History contains one tool message per call, in call order.
    let tool_outputs: Vec<String> = agent
        .memory
        .messages()
        .iter()
        .filter_map(|m| match m {
            Message::Tool { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_outputs, vec!["result-1", "result-2", "result-3"]);
}

/// Malformed calls (invalid JSON arguments, empty tool name) must still get a
/// tool message in history — an assistant turn with unanswered tool_calls is
/// rejected by strict providers — and the loop must continue to a final
/// answer instead of stalling.
#[tokio::test]
async fn test_malformed_tool_calls_still_record_tool_results() {
    let responses = vec![
        Message::Assistant {
            content: None,
            reasoning: None,
            replay: None,
            tool_calls: Some(vec![
                ToolCall {
                    id: "call_bad_json".to_string(),
                    r#type: "function".to_string(),
                    function: FunctionCall {
                        name: "mock_tool".to_string(),
                        arguments: "not-json".to_string(),
                    },
                },
                ToolCall {
                    id: "call_empty_name".to_string(),
                    r#type: "function".to_string(),
                    function: FunctionCall {
                        name: String::new(),
                        arguments: "{}".to_string(),
                    },
                },
            ]),
        },
        Message::Assistant {
            content: Some("recovered".to_string()),
            reasoning: None,
            replay: None,
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

    let mut finish: Vec<(String, String)> = Vec::new();
    let mut final_response: Option<String> = None;
    {
        let mut stream = Box::pin(agent.ask("Trigger malformed calls"));
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                AgentEvent::ToolFinish { tool, output, .. } => {
                    finish.push((tool, output));
                }
                AgentEvent::Finish { response } => final_response = Some(response),
                _ => {}
            }
        }
    }

    // The loop continued to a final answer after the malformed calls.
    assert_eq!(final_response.as_deref(), Some("recovered"));

    // Both malformed calls produced a ToolFinish with an error output.
    assert_eq!(finish.len(), 2);
    assert_eq!(finish[0].0, "mock_tool");
    assert!(finish[0].1.contains("invalid arguments"));
    assert!(finish[1].1.contains("empty name"));

    // History contains one answered tool message per tool_call, including
    // the malformed ones.
    let tool_messages: Vec<(String, String)> = agent
        .memory
        .messages()
        .iter()
        .filter_map(|m| match m {
            Message::Tool {
                tool_call_id,
                content,
                ..
            } => Some((tool_call_id.clone(), content.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(tool_messages.len(), 2);
    assert!(tool_messages.iter().any(|(id, _)| id == "call_bad_json"));
    assert!(tool_messages.iter().any(|(id, _)| id == "call_empty_name"));
}
