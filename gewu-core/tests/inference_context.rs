//! Per-request application context: transient text, pinned tool metadata.

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use futures::StreamExt;
use gewu_core::traits::{InferenceContext, InferenceContextSource, LlmProvider, Tool};
use gewu_core::types::{AgentEvent, Message, ProviderEvent, ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde_json::{Value, json};

/// A source that reports an increasing context revision per request.
struct CountingSource {
    calls: AtomicUsize,
    seen: Arc<Mutex<Vec<BTreeMap<String, Value>>>>,
}

#[async_trait]
impl InferenceContextSource for CountingSource {
    async fn inference_context(&self) -> Result<InferenceContext, Box<dyn Error + Send + Sync>> {
        let revision = self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(BTreeMap::from([(
            "context_ref".to_string(),
            json!(format!("r{revision}")),
        )]));
        Ok(InferenceContext {
            text: format!("bound datasets: [] (revision {revision})"),
            metadata: BTreeMap::from([("context_ref".to_string(), json!(format!("r{revision}")))]),
        })
    }
}

/// Reports the `context_ref` it received, so we can prove propagation.
struct RefTool {
    observed: Arc<Mutex<Vec<Option<String>>>>,
}

#[async_trait]
impl Tool for RefTool {
    fn name(&self) -> &str {
        "ref_probe"
    }
    fn description(&self) -> &str {
        "reports the pinned context ref"
    }
    fn schema(&self) -> RootSchema {
        serde_json::from_value(json!({"type":"object"})).unwrap()
    }
    async fn execute(
        &self,
        _args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let seen = ctx
            .metadata
            .get("context_ref")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.observed.lock().unwrap().push(seen);
        Ok(ToolResult::new("ref_probe", "ok"))
    }
}

/// Calls the tool once, then answers. Records every request's messages.
struct ScriptedLlm {
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    round: AtomicUsize,
}

#[async_trait]
impl LlmProvider for ScriptedLlm {
    fn build(
        &self,
        messages: &[Message],
        _tools: &[Arc<dyn Tool>],
    ) -> Result<Value, Box<dyn Error + Send + Sync>> {
        self.requests.lock().unwrap().push(messages.to_vec());
        Ok(json!({}))
    }

    async fn send<'a>(
        &'a self,
        _request: Value,
    ) -> Result<
        futures::stream::BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        let round = self.round.fetch_add(1, Ordering::SeqCst);
        let events: Vec<Result<ProviderEvent, Box<dyn Error + Send + Sync>>> = if round == 0 {
            vec![
                Ok(ProviderEvent::ToolCallStart {
                    id: "c1".into(),
                    name: "ref_probe".into(),
                }),
                Ok(ProviderEvent::ToolCallDelta {
                    id: "c1".into(),
                    delta: "{}".into(),
                }),
                Ok(ProviderEvent::Finish {
                    reason: Some("tool_calls".into()),
                }),
            ]
        } else {
            vec![
                Ok(ProviderEvent::ContentDelta {
                    delta: "done".into(),
                }),
                Ok(ProviderEvent::Finish {
                    reason: Some("stop".into()),
                }),
            ]
        };
        Ok(futures::stream::iter(events).boxed())
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn each_request_gets_one_transient_context_and_pinned_metadata() {
    let source = Arc::new(CountingSource {
        calls: AtomicUsize::new(0),
        seen: Arc::new(Mutex::new(Vec::new())),
    });
    let observed = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut agent = gewu_core::Agent::builder()
        .llm(Arc::new(ScriptedLlm {
            requests: requests.clone(),
            round: AtomicUsize::new(0),
        }))
        .with_tools(vec![Arc::new(RefTool {
            observed: observed.clone(),
        })])
        .system_prompt("system")
        .with_inference_context(source.clone())
        .build()
        .unwrap();

    let events: Vec<AgentEvent> = agent
        .ask("go")
        .filter_map(|e| async move { e.ok() })
        .collect()
        .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Finish { .. })),
        "{events:?}"
    );

    // Two model requests, each with exactly one live block and one revision.
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "expected two provider requests");
    let mut revisions = Vec::new();
    for request in requests.iter() {
        let text = serde_json::to_string(request).unwrap();
        assert_eq!(
            text.matches("application context").count(),
            1,
            "each request carries exactly one live block"
        );
        for revision in ["revision 0", "revision 1"] {
            if text.contains(revision) {
                revisions.push(revision);
            }
        }
    }
    assert_eq!(revisions, vec!["revision 0", "revision 1"]);

    // The tool saw the revision of the request that produced it.
    assert_eq!(
        observed.lock().unwrap().as_slice(),
        &[Some("r0".to_string())]
    );

    // Live context is not durable memory.
    let memory = serde_json::to_string(agent.memory.messages()).unwrap();
    assert!(
        !memory.contains("application context"),
        "live context leaked into memory"
    );
}

#[tokio::test]
async fn a_failing_source_does_not_block_the_turn() {
    struct Broken;
    #[async_trait]
    impl InferenceContextSource for Broken {
        async fn inference_context(
            &self,
        ) -> Result<InferenceContext, Box<dyn Error + Send + Sync>> {
            Err("provider offline".into())
        }
    }

    let mut agent = gewu_core::Agent::builder()
        .llm(Arc::new(Vec::<ProviderEvent>::new().into_llm()))
        .system_prompt("system")
        .with_inference_context(Arc::new(Broken))
        .build()
        .unwrap();
    let events: Vec<AgentEvent> = agent
        .ask("go")
        .filter_map(|e| async move { e.ok() })
        .collect()
        .await;
    assert!(
        events.iter().any(|event| matches!(event, AgentEvent::Status { message } if message.contains("context unavailable"))),
        "{events:?}"
    );
}

/// Minimal no-op provider for the failure test.
trait IntoLlm {
    fn into_llm(self) -> ScriptedLlm;
}

impl IntoLlm for Vec<ProviderEvent> {
    fn into_llm(self) -> ScriptedLlm {
        ScriptedLlm {
            requests: Arc::new(Mutex::new(Vec::new())),
            round: AtomicUsize::new(0),
        }
    }
}
