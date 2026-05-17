use std::error::Error;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use futures::StreamExt;
use futures::stream::BoxStream;
use gewu_canvas::invocation_poller::PendingInvocation;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{
    AgentEvent, FunctionCall, Message, ProviderEvent, SpecialistExecutionRequest, ToolCall,
};
use gewu_runtime::{GeWuRuntime, InvocationExecutor, SpecialistExecutionEvent};
use serde_json::{Value, json};
use sqlx::sqlite::SqlitePoolOptions;

#[derive(Clone)]
struct CanvasHarnessState {
    pending: Arc<Mutex<Vec<PendingInvocation>>>,
    events: Arc<Mutex<Vec<Value>>>,
}

async fn get_pending(State(state): State<CanvasHarnessState>) -> Json<Value> {
    let mut pending = state.pending.lock().unwrap();
    let invocations = pending
        .drain(..)
        .map(|invocation| {
            json!({
                "invocation_id": invocation.invocation_id,
                "agent_id": invocation.agent_id,
                "briefing": invocation.briefing,
                "selected_nodes": invocation.selected_nodes,
            })
        })
        .collect::<Vec<_>>();
    Json(Value::Array(invocations))
}

async fn post_event(
    State(state): State<CanvasHarnessState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let mut events = state.events.lock().unwrap();
    events.push(body);
    Json(json!({ "ok": true, "event_id": format!("evt_{}", events.len()) }))
}

async fn get_events(State(state): State<CanvasHarnessState>) -> Json<Value> {
    let events = state
        .events
        .lock()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(idx, event)| {
            json!({
                "id": format!("evt_{}", idx + 1),
                "kind": event.get("kind").cloned().unwrap_or(Value::Null),
                "payload": event.get("payload").cloned().unwrap_or(Value::Null),
                "timestamp": "2026-05-17T00:00:00.000Z",
            })
        })
        .collect::<Vec<_>>();
    Json(json!({ "events": events }))
}

async fn start_canvas_harness(invocation: PendingInvocation) -> (String, Arc<Mutex<Vec<Value>>>) {
    let state = CanvasHarnessState {
        pending: Arc::new(Mutex::new(vec![invocation])),
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let events = state.events.clone();
    let app = Router::new()
        .route("/api/workflow/invocations/pending", get(get_pending))
        .route("/api/workflow/events", get(get_events).post(post_event))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{}", addr), events)
}

struct ScriptedLlm {
    responses: Arc<Mutex<Vec<Message>>>,
    seen_user_prompts: Arc<Mutex<Vec<String>>>,
    seen_tool_names: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait]
impl LlmProvider for ScriptedLlm {
    async fn stream_response<'a>(
        &'a self,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        self.seen_tool_names
            .lock()
            .unwrap()
            .push(tools.iter().map(|tool| tool.name().to_string()).collect());
        if let Some(Message::User { content }) = messages
            .iter()
            .rev()
            .find(|message| matches!(message, Message::User { .. }))
        {
            self.seen_user_prompts
                .lock()
                .unwrap()
                .push(serde_json::to_string(content).unwrap());
        }

        let response = self.responses.lock().unwrap().remove(0);
        let stream = async_stream::try_stream! {
            if let Message::Assistant { content, tool_calls } = response {
                if let Some(content) = content {
                    yield ProviderEvent::ContentDelta { delta: content };
                }
                if let Some(calls) = tool_calls {
                    for call in calls {
                        yield ProviderEvent::ToolCallStart { id: call.id.clone(), name: call.function.name };
                        yield ProviderEvent::ToolCallDelta { id: call.id, delta: call.function.arguments };
                    }
                }
            }
        };
        Ok(Box::pin(stream))
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec!["scripted-sm30-agent".to_string()])
    }
}

fn sm30_workflow_events() -> Vec<Value> {
    vec![
        json!({"kind":"data_ingested","payload":{"data_id":"samples_csv","uri":"/data/meta/samples.csv","format":"csv","metadata":{"summary":"metadata source"}}}),
        json!({"kind":"data_ingested","payload":{"data_id":"pu1_peaks_bed","uri":"/results/CUTTAG_PU.1_merged_peaks.bed","format":"bed","metadata":{"summary":"CUTTAG PU.1 merged peaks BED"}}}),
        json!({"kind":"data_ingested","payload":{"data_id":"atac_peaks_bed","uri":"/results/ATAC_merged_peaks_v2.bed","format":"bed","metadata":{"summary":"ATAC merged peaks v2 BED"}}}),
        json!({"kind":"data_ingested","payload":{"data_id":"ucf_SM30_1","uri":"/data/SM30-1_CUT22.sorted.markd.ucf","format":"ucf","metadata":{"sample":"SM30-1","summary":"unique fragments=24,366,015"}}}),
        json!({"kind":"data_ingested","payload":{"data_id":"ucf_SM30_2","uri":"/data/SM30-2_CUT22.sorted.markd.ucf","format":"ucf","metadata":{"sample":"SM30-2","summary":"unique fragments=23,628,426"}}}),
        json!({"kind":"step_executed","payload":{"step_id":"step_metadata_SM30","inputs":["samples_csv"],"outputs":["sm30_samples"],"code":"python build_pu1_manifest.py --group SM30","language":"python","parameters":{"group":"SM30","assay":"PU.1"},"duration_ms":0,"exit_code":0}}),
        json!({"kind":"artifact_produced","payload":{"artifact_id":"sm30_samples","produced_by":"step_metadata_SM30","uri":"/process/sm30_manifest.tsv","format":"tsv","summary":"SM30 samples: 2 PU.1 replicates"}}),
        json!({"kind":"step_executed","payload":{"step_id":"step_qc_SM30","inputs":["ucf_SM30_1","ucf_SM30_2","pu1_peaks_bed"],"outputs":["qc_table_SM30"],"code":"tachyon ucf-qc --assay CUTTAG_PU.1 --peak-bed CUTTAG_PU.1_merged_peaks.bed SM30-1.ucf SM30-2.ucf","language":"bash","parameters":{"peak_set":"CUTTAG_PU.1_merged_peaks.bed"},"duration_ms":4728,"exit_code":0}}),
        json!({"kind":"artifact_produced","payload":{"artifact_id":"qc_table_SM30","produced_by":"step_qc_SM30","uri":"/result/sm30_qc_table.tsv","format":"tsv","summary":"SM30 QC: unique 24M/24M; PU.1 FRiP 0.0497/0.0495"}}),
        json!({"kind":"step_executed","payload":{"step_id":"step_repro_SM30","inputs":["qc_table_SM30","atac_peaks_bed"],"outputs":["repro_SM30"],"code":"tachyon ucf-qc --assay CUTTAG_PU.1 --peak-bed ATAC_merged_peaks_v2.bed --write-count-matrix SM30-1.ucf SM30-2.ucf","language":"bash","parameters":{"method":"spearman","peak_set":"ATAC_merged_peaks_v2.bed"},"duration_ms":2283,"exit_code":0}}),
        json!({"kind":"artifact_produced","payload":{"artifact_id":"repro_SM30","produced_by":"step_repro_SM30","uri":"/result/sm30_reproducibility.json","format":"json","summary":"SM30 reproducibility: Spearman r=0.576"}}),
        json!({"kind":"edge_declared","payload":{"from_id":"ucf_SM30_1","to_id":"qc_table_SM30","relationship":"input_to"}}),
        json!({"kind":"edge_declared","payload":{"from_id":"ucf_SM30_2","to_id":"qc_table_SM30","relationship":"input_to"}}),
        json!({"kind":"edge_declared","payload":{"from_id":"qc_table_SM30","to_id":"repro_SM30","relationship":"derived_from"}}),
        json!({"kind":"artifact_surfaced","payload":{"artifact_id":"sm30_samples","canvas_node_id":"cn_sm30_samples","position":{"x":0,"y":-120}}}),
        json!({"kind":"artifact_surfaced","payload":{"artifact_id":"ucf_SM30_1","canvas_node_id":"cn_ucf_SM30_1","position":{"x":0,"y":40}}}),
        json!({"kind":"artifact_surfaced","payload":{"artifact_id":"ucf_SM30_2","canvas_node_id":"cn_ucf_SM30_2","position":{"x":0,"y":180}}}),
        json!({"kind":"artifact_surfaced","payload":{"artifact_id":"qc_table_SM30","canvas_node_id":"cn_qc_SM30","position":{"x":320,"y":110}}}),
        json!({"kind":"artifact_surfaced","payload":{"artifact_id":"repro_SM30","canvas_node_id":"cn_repro_SM30","position":{"x":640,"y":110}}}),
    ]
}

fn sm30_tool_calls(session_id: &str) -> Vec<ToolCall> {
    sm30_workflow_events()
        .into_iter()
        .enumerate()
        .map(|(idx, mut event)| {
            let object = event.as_object_mut().unwrap();
            object.insert(
                "session_id".to_string(),
                Value::String(session_id.to_string()),
            );
            ToolCall {
                id: format!("emit_{idx}"),
                r#type: "function".to_string(),
                function: FunctionCall {
                    name: "workflow.emit".to_string(),
                    arguments: serde_json::to_string(&event).unwrap(),
                },
            }
        })
        .collect::<Vec<_>>()
}

struct ScriptedRunEvidence {
    tool_finish_count: usize,
    observation_count: usize,
    saw_repro_observation: bool,
    completed: bool,
}

async fn run_scripted_canvas_analyst(
    canvas_url: &str,
    prompt: String,
    session_id: &str,
) -> ScriptedRunEvidence {
    unsafe {
        std::env::set_var("GEWU_TACHYON_CANVAS_URL", canvas_url);
    }

    let tool_calls = sm30_tool_calls(session_id);
    assert_eq!(tool_calls.len(), 19);

    let seen_user_prompts = Arc::new(Mutex::new(Vec::new()));
    let seen_tool_names = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedLlm {
        responses: Arc::new(Mutex::new(vec![
            Message::Assistant {
                content: None,
                tool_calls: Some(tool_calls),
            },
            Message::Assistant {
                content: Some("SM30 PU.1 QC complete".to_string()),
                tool_calls: None,
            },
        ])),
        seen_user_prompts: seen_user_prompts.clone(),
        seen_tool_names: seen_tool_names.clone(),
    });

    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    gewu_storage::run_migrations(&pool).await.unwrap();
    let runtime = GeWuRuntime::builder()
        .provider(provider)
        .pool(pool)
        .register_builtin_capabilities()
        .register_builtin_specialists()
        .build()
        .unwrap();

    let request = SpecialistExecutionRequest {
        task_id: format!("task_{session_id}"),
        specialist_id: "canvas-analyst".to_string(),
        session_id: session_id.to_string(),
        prompt,
        workdir: Some(std::env::current_dir().unwrap().display().to_string()),
        metadata_json: json!({}),
    };

    let mut stream = runtime.execute_request_stream(request, Vec::new());
    let mut evidence = ScriptedRunEvidence {
        tool_finish_count: 0,
        observation_count: 0,
        saw_repro_observation: false,
        completed: false,
    };

    while let Some(event) = stream.next().await {
        match event.unwrap() {
            SpecialistExecutionEvent::Agent(AgentEvent::ToolFinish { tool, .. })
                if tool == "workflow.emit" =>
            {
                evidence.tool_finish_count += 1;
            }
            SpecialistExecutionEvent::Agent(AgentEvent::Observation { payload, .. }) => {
                evidence.observation_count += 1;
                if payload.to_string().contains("repro_SM30") {
                    evidence.saw_repro_observation = true;
                }
            }
            SpecialistExecutionEvent::Completed(envelope) => {
                evidence.completed = true;
                assert_eq!(envelope.specialist_id, "canvas-analyst");
            }
            _ => {}
        }
    }

    assert!(
        seen_user_prompts
            .lock()
            .unwrap()
            .iter()
            .any(|prompt| prompt.contains("帮我对 SM30 组的 PU.1 样本做 QC"))
    );
    assert!(
        seen_tool_names
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .any(|name| name == "workflow.emit")
    );

    evidence
}

#[tokio::test]
async fn canvas_analyst_polls_invocation_emits_19_events_and_observes_canvas() {
    let invocation = PendingInvocation {
        invocation_id: "inv_sm30".to_string(),
        agent_id: "canvas-analyst".to_string(),
        briefing: "## Trigger\nUser prompt: 帮我对 SM30 组的 PU.1 样本做 QC\n## Selection\n- ucf_SM30_1\n- ucf_SM30_2".to_string(),
        selected_nodes: vec!["ucf_SM30_1".to_string(), "ucf_SM30_2".to_string()],
    };
    let (canvas_url, posted_events) = start_canvas_harness(invocation).await;
    unsafe {
        std::env::set_var("GEWU_TACHYON_CANVAS_URL", &canvas_url);
    }

    let executor = InvocationExecutor::new(&canvas_url, 10);
    let pending = executor.poll_once().await;
    assert_eq!(pending.len(), 1);
    let prompt = executor.build_prompt(&pending[0]);
    assert!(prompt.contains("帮我对 SM30 组的 PU.1 样本做 QC"));

    let evidence = run_scripted_canvas_analyst(&canvas_url, prompt, "sm30_agent_loop").await;

    assert_eq!(posted_events.lock().unwrap().len(), 19);
    assert_eq!(evidence.tool_finish_count, 19);
    assert!(
        evidence.observation_count >= 19,
        "expected observation events after workflow.emit calls"
    );
    assert!(
        evidence.saw_repro_observation,
        "expected observation payload to include repro_SM30"
    );
    assert!(evidence.completed);
}

#[tokio::test]
#[ignore = "requires a running tachyon-canvas server URL in GEWU_LIVE_TACHYON_CANVAS_URL"]
async fn live_tachyon_canvas_server_prompt_to_agent_loop() {
    let canvas_url = std::env::var("GEWU_LIVE_TACHYON_CANVAS_URL")
        .expect("set GEWU_LIVE_TACHYON_CANVAS_URL to a running tachyon-canvas server");
    let client = reqwest::Client::new();
    let _ = client
        .delete(format!("{canvas_url}/api/canvas/graph"))
        .send()
        .await
        .unwrap();
    let _ = client
        .get(format!("{canvas_url}/api/workflow/invocations/pending"))
        .send()
        .await
        .unwrap();

    client
        .post(format!("{canvas_url}/api/agent/subscribe"))
        .json(&json!({
            "agent_id": "canvas-analyst",
            "pattern": { "by_kind": ["user_interaction"] }
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    client
        .post(format!("{canvas_url}/api/workflow/events"))
        .json(&json!({
            "kind": "user_interaction",
            "actor": "user",
            "session_id": "sm30_live_agent_invocation",
            "payload": {
                "interaction_id": "prompt_sm30_pu1_qc",
                "kind": "prompt_submitted",
                "target_nodes": ["ucf_SM30_1", "ucf_SM30_2"],
                "data": { "prompt": "帮我对 SM30 组的 PU.1 样本做 QC" }
            }
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let executor = InvocationExecutor::new(&canvas_url, 10);
    let pending = executor.poll_once().await;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].agent_id, "canvas-analyst");
    assert_eq!(pending[0].selected_nodes, ["ucf_SM30_1", "ucf_SM30_2"]);
    let prompt = executor.build_prompt(&pending[0]);
    assert!(prompt.contains("帮我对 SM30 组的 PU.1 样本做 QC"));

    let evidence = run_scripted_canvas_analyst(&canvas_url, prompt, "sm30_live_agent_loop").await;
    assert_eq!(evidence.tool_finish_count, 19);
    assert!(evidence.observation_count >= 19);
    assert!(evidence.saw_repro_observation);
    assert!(evidence.completed);

    let graph: Value = client
        .get(format!("{canvas_url}/api/canvas/graph"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(graph["nodes"].as_array().unwrap().len() >= 5);
    assert!(graph["edges"].as_array().unwrap().len() >= 3);

    let lineage: Value = client
        .get(format!("{canvas_url}/api/workflow/lineage/repro_SM30"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let lineage_ids = lineage["lineage"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["node_id"].as_str())
        .collect::<Vec<_>>();
    assert!(lineage_ids.contains(&"ucf_SM30_1"));
    assert!(lineage_ids.contains(&"ucf_SM30_2"));
    assert!(lineage_ids.contains(&"qc_table_SM30"));
}
