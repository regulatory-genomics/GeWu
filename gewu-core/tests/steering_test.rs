//! Steering integration tests: user messages sent through the
//! [`Steering`](gewu_core::Steering) handle while a run is in progress are
//! injected at round boundaries, never between an assistant turn and its
//! tool results.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use gewu_core::Steering;
use gewu_core::agent::AgentBuilder;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{
    AgentEvent, ContentPart, FunctionCall, Message, ProviderEvent, ToolCall, ToolContext,
    ToolResult,
};
use serde_json::Value;
use std::error::Error;
use std::sync::{Arc, Mutex};

/// Streams scripted assistant messages and captures every request's
/// message list for assertions.
struct ScriptedLlm {
    responses: Mutex<Vec<Message>>,
    captured: Mutex<Vec<Vec<Message>>>,
}

#[derive(Debug)]
struct MockError(String);
impl std::fmt::Display for MockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl Error for MockError {}

impl ScriptedLlm {
    fn new(responses: Vec<Message>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses),
            captured: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<Vec<Message>> {
        self.captured.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for ScriptedLlm {
    fn build(
        &self,
        messages: &[Message],
        _tools: &[Arc<dyn Tool>],
    ) -> Result<Value, Box<dyn Error + Send + Sync>> {
        self.captured.lock().unwrap().push(messages.to_vec());
        Ok(serde_json::json!({ "model": "mock" }))
    }

    async fn send<'a>(
        &'a self,
        _request: Value,
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        let response = {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err(Box::new(MockError("No more mock responses".to_string())));
            }
            responses.remove(0)
        };
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

/// Sleeps for `ms` milliseconds, then echoes its id.
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
async fn background_resume_is_user_level_data() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let llm = ScriptedLlm::new(vec![Message::Assistant {
            content: Some("done".into()),
            reasoning: None,
            replay: None,
            tool_calls: None,
        }]);
        let mut agent = AgentBuilder::default()
            .llm(llm.clone())
            .system_prompt("System")
            .build()
            .unwrap();
        agent
            .steering()
            .notify(gewu_core::agent::BackgroundNotification {
                source: "workflow".into(),
                id: "run".into(),
                status: "completed".into(),
                output: "untrusted result".into(),
            });
        let events: Vec<_> = agent.resume().collect().await;
        assert!(events.iter().all(Result::is_ok));
        let requests = llm.requests();
        assert_eq!(roles(&requests[0]), vec!["system", "user"]);
        assert!(user_texts(&requests[0])[0].contains("untrusted result"));
    })
    .await
    .expect("background resume timed out");
}

fn roles(messages: &[Message]) -> Vec<&'static str> {
    messages
        .iter()
        .map(|m| match m {
            Message::Compaction { .. } => "compaction",
            Message::System { .. } => "system",
            Message::User { .. } => "user",
            Message::Assistant { .. } => "assistant",
            Message::Tool { .. } => "tool",
        })
        .collect()
}

fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(
                content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn background_delivery_at_finish_boundary_resumes_without_user_input() {
    let answer = || Message::Assistant {
        content: Some("answer".into()),
        reasoning: None,
        replay: None,
        tool_calls: None,
    };
    let llm = ScriptedLlm::new(vec![answer(), answer()]);
    let mut agent = AgentBuilder::default()
        .llm(llm.clone())
        .system_prompt("System")
        .build()
        .unwrap();
    let inbox = agent.steering().clone();
    let mut stream = Box::pin(agent.ask("start"));
    while let Some(event) = stream.next().await {
        if matches!(event.unwrap(), AgentEvent::Finish { .. }) {
            break;
        }
    }
    inbox.notify(gewu_core::agent::BackgroundNotification {
        source: "workflow".into(),
        id: "run".into(),
        status: "completed".into(),
        output: "result".into(),
    });
    drop(stream);
    let mut stream = Box::pin(agent.resume());
    while let Some(event) = stream.next().await {
        event.unwrap();
    }
    drop(stream);
    assert_eq!(llm.requests().len(), 2);
    assert_eq!(
        user_texts(&llm.requests()[1]),
        vec!["start", "Background workflow run: completed\nresult"]
    );
    assert!(
        llm.requests()[1]
            .iter()
            .any(|m| matches!(m, Message::User { content } if content.iter().any(|part| matches!(part, ContentPart::Text { text } if text.contains("workflow run")))))
    );
    assert!(!inbox.is_pending());
}

fn tool_call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: "call_1".to_string(),
        r#type: "function".to_string(),
        function: FunctionCall {
            name: name.to_string(),
            arguments: arguments.to_string(),
        },
    }
}

/// A steer sent while a tool is executing is injected after the tool
/// results are recorded and before the next LLM request: the provider
/// sees the tool result, then the steered user message — never a user
/// message wedged between the assistant turn and its tool results.
#[tokio::test]
async fn steer_during_tool_execution_injects_at_round_boundary() {
    let llm = ScriptedLlm::new(vec![
        Message::Assistant {
            content: None,
            reasoning: None,
            replay: None,
            tool_calls: Some(vec![tool_call(
                "slow_tool",
                serde_json::json!({ "id": 1, "ms": 250 }),
            )]),
        },
        Message::Assistant {
            content: Some("steered answer".to_string()),
            reasoning: None,
            replay: None,
            tool_calls: None,
        },
    ]);

    let mut agent = AgentBuilder::default()
        .llm(llm.clone())
        .with_tools(vec![Arc::new(SlowTool) as Arc<dyn Tool>])
        .system_prompt("System")
        .build()
        .unwrap();

    let steering: Steering = agent.steering().clone();
    let mut stream = Box::pin(agent.ask("run the tool"));

    // Advance until the tool is executing.
    loop {
        match stream.next().await {
            Some(Ok(AgentEvent::ToolStart { tool, .. })) if tool == "slow_tool" => break,
            Some(Ok(_)) => {}
            Some(Err(e)) => panic!("agent error before ToolStart: {e}"),
            None => panic!("stream ended before ToolStart"),
        }
    }
    steering.send("focus on the fast path").unwrap();

    let mut steered = Vec::new();
    let mut finish = None;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            AgentEvent::Steered { text } => steered.push(text),
            AgentEvent::Finish { response } => finish = Some(response),
            _ => {}
        }
    }

    assert_eq!(steered, vec!["focus on the fast path"]);
    assert_eq!(finish.as_deref(), Some("steered answer"));

    drop(stream);

    let requests = llm.requests();
    assert_eq!(requests.len(), 2);
    // The steered message rides AFTER the tool result, never inside it.
    assert_eq!(
        roles(&requests[1]),
        vec!["system", "user", "assistant", "tool", "user"]
    );
    assert_eq!(user_texts(&requests[1])[1], "focus on the fast path");

    // And it is persisted as part of the transcript.
    assert_eq!(
        roles(agent.memory.messages()),
        vec!["system", "user", "assistant", "tool", "user", "assistant"]
    );
}

/// A steer arriving while the model streams its final answer extends the
/// run: instead of finishing, the agent keeps going and answers the
/// steered message in the same conversation run.
#[tokio::test]
async fn steer_during_final_answer_extends_the_run() {
    let llm = ScriptedLlm::new(vec![
        Message::Assistant {
            content: Some("first draft".to_string()),
            reasoning: None,
            replay: None,
            tool_calls: None,
        },
        Message::Assistant {
            content: Some("steered answer".to_string()),
            reasoning: None,
            replay: None,
            tool_calls: None,
        },
    ]);

    let mut agent = AgentBuilder::default()
        .llm(llm.clone())
        .system_prompt("System")
        .build()
        .unwrap();

    let steering = agent.steering().clone();
    let mut stream = Box::pin(agent.ask("start"));

    // First content delta is delivered; the stream still holds the Finish
    // event, so steering now lands before the run's completion check.
    match stream.next().await {
        Some(Ok(AgentEvent::ContentDelta { delta })) => assert_eq!(delta, "first draft"),
        other => panic!("expected ContentDelta, got {other:?}"),
    }
    steering.send("now answer this").unwrap();

    let mut steered = Vec::new();
    let mut finish = None;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            AgentEvent::Steered { text } => steered.push(text),
            AgentEvent::Finish { response } => finish = Some(response),
            _ => {}
        }
    }

    assert_eq!(steered, vec!["now answer this"]);
    assert_eq!(finish.as_deref(), Some("steered answer"));

    let requests = llm.requests();
    assert_eq!(
        requests.len(),
        2,
        "the run continued after the first answer"
    );
    assert_eq!(
        roles(&requests[1]),
        vec!["system", "user", "assistant", "user"]
    );
    assert_eq!(user_texts(&requests[1]), vec!["start", "now answer this"]);
}

/// Messages steered while the agent is idle are delivered at the start of
/// the next ask, ahead of that ask's own prompt, preserving send order.
#[tokio::test]
async fn steer_while_idle_is_delivered_at_next_ask_start() {
    let llm = ScriptedLlm::new(vec![Message::Assistant {
        content: Some("combined answer".to_string()),
        reasoning: None,
        replay: None,
        tool_calls: None,
    }]);

    let mut agent = AgentBuilder::default()
        .llm(llm.clone())
        .system_prompt("System")
        .build()
        .unwrap();

    agent.steering().send("earlier question").unwrap();

    let mut steered = Vec::new();
    let mut finish = None;
    let mut stream = Box::pin(agent.ask("main question"));
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            AgentEvent::Steered { text } => steered.push(text),
            AgentEvent::Finish { response } => finish = Some(response),
            _ => {}
        }
    }

    assert_eq!(steered, vec!["earlier question"]);
    assert_eq!(finish.as_deref(), Some("combined answer"));

    let requests = llm.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        user_texts(&requests[0]),
        vec!["earlier question", "main question"]
    );
}
