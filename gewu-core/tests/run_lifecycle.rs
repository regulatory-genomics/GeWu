//! Run lifecycle: call correlation, cancellation, budgets and protocol-valid
//! tool history.
//!
//! Every test here pins one guarantee the browser assistant depends on. A run
//! that stops must say so; a tool call must be answered; a caller must be able
//! to tell which tool invocation produced which result.

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{
    AgentEvent, FunctionCall, Message, ProviderEvent, ToolCall, ToolContext, ToolResult,
};
use gewu_core::{Agent, RunOptions};
use serde_json::Value;
use std::error::Error;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct MockError(String);
impl std::fmt::Display for MockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl Error for MockError {}

/// Streams one scripted assistant message per call and records what the agent
/// sent, so a test can assert on the history the provider actually saw.
struct ScriptedLlm {
    script: Mutex<Vec<Message>>,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

#[derive(Debug, Clone, Default)]
struct SeenRequest {
    tool_message_ids: Vec<String>,
}

impl ScriptedLlm {
    fn new(script: Vec<Message>) -> (Self, Arc<Mutex<Vec<SeenRequest>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                script: Mutex::new(script),
                seen: seen.clone(),
            },
            seen,
        )
    }
}

#[async_trait]
impl LlmProvider for ScriptedLlm {
    fn build(
        &self,
        messages: &[Message],
        _tools: &[Arc<dyn Tool>],
    ) -> Result<Value, Box<dyn Error + Send + Sync>> {
        let tool_message_ids = messages
            .iter()
            .filter_map(|message| match message {
                Message::Tool { tool_call_id, .. } => Some(tool_call_id.clone()),
                _ => None,
            })
            .collect();
        self.seen
            .lock()
            .unwrap()
            .push(SeenRequest { tool_message_ids });
        Ok(Value::Null)
    }

    async fn send<'a>(
        &'a self,
        _request: Value,
    ) -> Result<BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>, Box<dyn Error + Send + Sync>>
    {
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            return Err(Box::new(MockError("script exhausted".to_string())));
        }
        let response = script.remove(0);
        let s = async_stream::try_stream! {
            match response {
                Message::Assistant { content, tool_calls } => {
                    if let Some(text) = content {
                        yield ProviderEvent::ContentDelta { delta: text };
                    }
                    if let Some(calls) = tool_calls {
                        for call in calls {
                            yield ProviderEvent::ToolCallStart {
                                id: call.id.clone(),
                                name: call.function.name,
                            };
                            yield ProviderEvent::ToolCallDelta {
                                id: call.id,
                                delta: call.function.arguments,
                            };
                        }
                    }
                    yield ProviderEvent::Finish { reason: None };
                }
                other => Err(MockError(format!("unexpected scripted message: {other:?}")))?,
            }
        };
        Ok(Box::pin(s))
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec!["mock".to_string()])
    }
}

struct EchoTool {
    name: &'static str,
    seen_calls: Arc<Mutex<Vec<(String, String)>>>,
}

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "echoes its arguments"
    }
    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(serde_json::Value)
    }
    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        self.seen_calls
            .lock()
            .unwrap()
            .push((ctx.run_id.clone(), ctx.call_id.clone()));
        Ok(ToolResult::new(self.name, format!("echo: {args}")))
    }
}

fn assistant(content: Option<&str>, calls: Vec<(&str, &str, &str)>) -> Message {
    Message::Assistant {
        content: content.map(|text| text.to_string()),
        tool_calls: if calls.is_empty() {
            None
        } else {
            Some(
                calls
                    .into_iter()
                    .map(|(id, name, args)| ToolCall {
                        id: id.to_string(),
                        r#type: "function".to_string(),
                        function: FunctionCall {
                            name: name.to_string(),
                            arguments: args.to_string(),
                        },
                    })
                    .collect(),
            )
        },
    }
}

#[tokio::test]
async fn tool_start_and_finish_share_one_call_id() {
    let (llm, _seen) = ScriptedLlm::new(vec![
        assistant(None, vec![("call_a", "echo", r#"{"n":1}"#)]),
        assistant(Some("done"), vec![]),
    ]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::builder()
        .llm(Arc::new(llm))
        .with_tools(vec![Arc::new(EchoTool {
            name: "echo",
            seen_calls: calls.clone(),
        }) as Arc<dyn Tool>])
        .build()
        .unwrap();

    let mut stream = Box::pin(agent.ask("go"));
    let mut start_id = None;
    let mut finish_id = None;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            AgentEvent::ToolStart { call_id, .. } => start_id = Some(call_id),
            AgentEvent::ToolFinish { call_id, .. } => finish_id = Some(call_id),
            _ => {}
        }
    }

    assert_eq!(start_id.as_deref(), Some("call_a"));
    assert_eq!(finish_id.as_deref(), Some("call_a"));
    let seen = calls.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1, "call_a", "the tool sees its own call id");
    assert!(!seen[0].0.is_empty(), "the tool sees a run id");
}

#[tokio::test]
async fn malformed_tool_arguments_still_answer_the_call() {
    let (llm, seen) = ScriptedLlm::new(vec![
        // Not valid JSON: execute_tool_call fails to parse the arguments.
        assistant(None, vec![("call_bad", "echo", "not-json")]),
        assistant(Some("recovered"), vec![]),
    ]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::builder()
        .llm(Arc::new(llm))
        .with_tools(vec![Arc::new(EchoTool {
            name: "echo",
            seen_calls: calls,
        }) as Arc<dyn Tool>])
        .build()
        .unwrap();

    let mut stream = Box::pin(agent.ask("go"));
    let mut finish = None;
    let mut error = None;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            AgentEvent::ToolFinish { output, .. } => {
                assert!(output.starts_with("Error:"), "got {output}");
            }
            AgentEvent::Error { error: message } => error = Some(message),
            AgentEvent::Finish { response } => finish = Some(response),
            _ => {}
        }
    }

    assert!(error.is_some(), "the failure is still reported");
    assert_eq!(finish.as_deref(), Some("recovered"), "the run continues");

    // The provider's second request must contain the tool answer, otherwise a
    // real provider rejects the whole conversation.
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].tool_message_ids, vec!["call_bad".to_string()]);
}

#[tokio::test]
async fn unknown_tool_also_answers_the_call() {
    let (llm, seen) = ScriptedLlm::new(vec![
        assistant(None, vec![("call_ghost", "does_not_exist", "{}")]),
        assistant(Some("ok"), vec![]),
    ]);
    let mut agent = Agent::builder().llm(Arc::new(llm)).build().unwrap();

    let mut stream = Box::pin(agent.ask("go"));
    while let Some(event) = stream.next().await {
        event.unwrap();
    }

    let requests = seen.lock().unwrap();
    assert_eq!(requests[1].tool_message_ids, vec!["call_ghost".to_string()]);
}

#[tokio::test]
async fn a_stale_cancel_does_not_poison_the_next_run() {
    let (llm, seen) = ScriptedLlm::new(vec![assistant(Some("ran"), vec![])]);
    let mut agent = Agent::builder().llm(Arc::new(llm)).build().unwrap();
    // No run is streaming, so this cancel has no effect.
    agent.cancel_handle().cancel();

    let mut stream = Box::pin(agent.ask("go"));
    let mut last = None;
    while let Some(event) = stream.next().await {
        last = Some(event.unwrap());
    }

    assert!(matches!(last, Some(AgentEvent::Finish { .. })));
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cancelling_mid_stream_ends_with_cancelled() {
    let (llm, _seen) = ScriptedLlm::new(vec![assistant(Some("chunk"), vec![])]);
    let mut agent = Agent::builder().llm(Arc::new(llm)).build().unwrap();
    let handle = agent.cancel_handle();

    let mut stream = Box::pin(agent.ask("go"));
    let first = stream.next().await.unwrap().unwrap();
    assert!(matches!(first, AgentEvent::ContentDelta { .. }));
    handle.cancel();

    let mut terminal = None;
    while let Some(event) = stream.next().await {
        terminal = Some(event.unwrap());
    }

    assert!(matches!(terminal, Some(AgentEvent::Cancelled { .. })));
}

#[tokio::test]
async fn the_tool_budget_is_a_terminal_error() {
    let (llm, _seen) = ScriptedLlm::new(vec![
        assistant(
            None,
            vec![
                ("call_1", "echo", "{}"),
                ("call_2", "echo", "{}"),
            ],
        ),
        assistant(Some("never"), vec![]),
    ]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::builder()
        .llm(Arc::new(llm))
        .with_tools(vec![Arc::new(EchoTool {
            name: "echo",
            seen_calls: calls.clone(),
        }) as Arc<dyn Tool>])
        .build()
        .unwrap();

    let mut stream = Box::pin(agent.run_with(
        "go",
        RunOptions {
            max_tool_calls: Some(1),
            ..RunOptions::default()
        },
    ));
    let mut last = None;
    while let Some(event) = stream.next().await {
        last = Some(event.unwrap());
    }

    match last {
        Some(AgentEvent::Error { error }) => assert!(error.contains("budget")),
        other => panic!("expected a terminal budget error, got {other:?}"),
    }
    assert_eq!(calls.lock().unwrap().len(), 1, "only one tool ran");
}

#[tokio::test]
async fn a_finished_run_reports_finish_not_cancelled() {
    let (llm, _seen) = ScriptedLlm::new(vec![assistant(Some("all done"), vec![])]);
    let mut agent = Agent::builder().llm(Arc::new(llm)).build().unwrap();

    let mut stream = Box::pin(agent.ask("go"));
    let mut last = None;
    while let Some(event) = stream.next().await {
        last = Some(event.unwrap());
    }

    match last {
        Some(AgentEvent::Finish { response }) => assert_eq!(response, "all done"),
        other => panic!("expected Finish, got {other:?}"),
    }
}
