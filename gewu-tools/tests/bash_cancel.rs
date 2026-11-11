//! A cancelled run must stop the process its `bash` call started.
//!
//! The agent only sets a flag; a tool that ignores it keeps writing into the
//! workspace while the UI already reports the run as stopped.

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{
    AgentEvent, FunctionCall, Message, ProviderEvent, ToolCall, ToolContext,
};
use gewu_core::Agent;
use gewu_tools::BashTool;
use serde_json::Value;
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::tempdir;

/// Asks for one `bash` call, then finishes if the agent somehow continues.
struct OneBashCall {
    command: String,
    sent: AtomicUsize,
}

#[async_trait]
impl LlmProvider for OneBashCall {
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
        let events = if self.sent.fetch_add(1, Ordering::SeqCst) == 0 {
            let call = ToolCall {
                id: "call_bash".to_string(),
                r#type: "function".to_string(),
                function: FunctionCall {
                    name: "bash".to_string(),
                    arguments: serde_json::json!({ "command": self.command }).to_string(),
                },
            };
            vec![
                Ok(ProviderEvent::ToolCallStart {
                    id: call.id.clone(),
                    name: "bash".to_string(),
                }),
                Ok(ProviderEvent::ToolCallDelta {
                    id: call.id.clone(),
                    delta: call.function.arguments.clone(),
                }),
                Ok(ProviderEvent::Finish { reason: None }),
            ]
        } else {
            vec![
                Ok(ProviderEvent::ContentDelta {
                    delta: "stopped".to_string(),
                }),
                Ok(ProviderEvent::Finish { reason: None }),
            ]
        };
        Ok(Box::pin(futures_util::stream::iter(events)))
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec!["mock".to_string()])
    }
}

#[tokio::test]
async fn cancelling_a_run_kills_the_bash_child_it_started() {
    let dir = tempdir().unwrap();
    let pid_file = dir.path().join("bash.pid");

    let tool = BashTool {
        working_directory: dir.path().to_path_buf(),
        safe_mode: false,
    };
    // Write the child's pid first, then block far past the test's timeout.
    let llm = OneBashCall {
        command: format!("echo $$ > {} ; sleep 30", pid_file.display()),
        sent: AtomicUsize::new(0),
    };
    let mut agent = Agent::builder()
        .llm(Arc::new(llm))
        .with_tools(vec![Arc::new(tool) as Arc<dyn Tool>])
        .build()
        .unwrap();

    let cancel = agent.cancel_handle();
    let started = Instant::now();
    let mut last = None;
    let mut cancelled = false;
    {
        let mut stream = Box::pin(agent.ask("go"));
        loop {
            tokio::select! {
                event = stream.next() => {
                    let Some(event) = event else { break };
                    let event = event.expect("the stream must not error");
                    let terminal = matches!(
                        event,
                        AgentEvent::Finish { .. }
                            | AgentEvent::Cancelled { .. }
                            | AgentEvent::Error { .. }
                    );
                    last = Some(event);
                    if terminal {
                        break;
                    }
                }
                // The tool is spawned only when the stream advances past the
                // announce loop, so the cancel has to come from here: wait
                // until bash has written its pid, then pull the plug.
                _ = tokio::time::sleep(Duration::from_millis(20)) => {
                    if !cancelled && pid_file.exists() {
                        cancelled = true;
                        cancel.cancel();
                    }
                    assert!(
                        started.elapsed() < Duration::from_secs(15),
                        "bash never started"
                    );
                }
            }
        }
    }

    assert!(cancelled, "the test never got to cancel a running bash");
    assert!(
        matches!(last, Some(AgentEvent::Cancelled { .. })),
        "expected a cancelled run, got {last:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the run waited for the 30 s command instead of cancelling it"
    );

    let pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("bash writes its pid before sleeping")
        .trim()
        .parse()
        .expect("pid is a number");
    for _ in 0..50 {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("bash child {pid} survived cancellation");
}

#[tokio::test]
async fn a_non_aborted_bash_call_still_reports_its_output() {
    let dir = tempdir().unwrap();
    let tool = BashTool {
        working_directory: dir.path().to_path_buf(),
        safe_mode: false,
    };
    let ctx = ToolContext::new("test", "msg");
    let result = tool
        .execute(serde_json::json!({ "command": "echo alive" }), &ctx)
        .await
        .expect("a short command still succeeds");

    assert!(result.output.contains("alive"), "{}", result.output);
}
