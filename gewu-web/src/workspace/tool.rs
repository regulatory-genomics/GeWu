use crate::workspace::{
    GraphMutation, SessionGraph, SessionGraphMutation, WorkflowEvent, WorkflowEventKind,
    WorkspaceRuntime,
};
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowEmitInput {
    pub kind: String,
    pub payload: Value,
    #[serde(default)]
    pub session_id: Option<String>,
}

pub struct InProcessWorkflowEmitTool {
    runtime: Arc<WorkspaceRuntime>,
}

impl InProcessWorkflowEmitTool {
    pub fn new(runtime: Arc<WorkspaceRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl Tool for InProcessWorkflowEmitTool {
    fn name(&self) -> &str {
        "workflow.emit"
    }

    fn description(&self) -> &str {
        "Record a workflow provenance event in the current GeWu workspace. Use artifact_produced previews and artifact_surfaced events to add canvas nodes."
    }

    fn schema(&self) -> RootSchema {
        schema_for!(WorkflowEmitInput)
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let input: WorkflowEmitInput = match serde_json::from_value(args) {
            Ok(input) => input,
            Err(error) => {
                return Ok(ToolResult::new(
                    self.name(),
                    json!({ "ok": false, "error": { "code": "invalid_input", "message": error.to_string() } }).to_string(),
                ));
            }
        };
        let kind_str = input.kind.clone();
        let kind: WorkflowEventKind = match serde_json::from_value(json!(kind_str)) {
            Ok(kind) => kind,
            Err(error) => {
                return Ok(ToolResult::new(
                    self.name(),
                    json!({ "ok": false, "error": { "code": "invalid_kind", "message": error.to_string() } }).to_string(),
                ));
            }
        };
        let mut event = WorkflowEvent {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            // The in-process web tool is workspace-bound by construction; the
            // active agent session is authoritative. Ignore model-provided
            // session_id so agents cannot write to the wrong canvas session.
            session_id: ctx.session_id.clone(),
            actor: "agent".to_string(),
            kind: kind.clone(),
            payload: normalize_payload_aliases(&input.kind, input.payload),
        };
        normalize_event_defaults(&mut event, &ctx.session_id);
        if let Err(error) = append_event_and_project(&self.runtime, event.clone()).await {
            return Ok(ToolResult::new(
                self.name(),
                json!({ "ok": false, "error": { "code": "invalid_event", "message": error.to_string() } }).to_string(),
            ));
        }
        Ok(ToolResult::new(
            self.name(),
            json!({"ok": true, "event_id": event.id, "event": event}).to_string(),
        ))
    }
}

pub fn normalize_payload_aliases(kind: &str, payload: Value) -> Value {
    let Value::Object(mut map) = payload else {
        return payload;
    };

    if let Some(id) = map.get("id").cloned() {
        let canonical = match kind {
            "data_ingested" => Some("data_id"),
            "step_executed" => Some("step_id"),
            "artifact_produced" | "artifact_surfaced" | "artifact_hidden" => Some("artifact_id"),
            "annotation_added" => Some("target_id"),
            _ => None,
        };
        if let Some(key) = canonical {
            map.entry(key.to_string()).or_insert(id);
        }
    }

    if kind == "edge_declared" {
        if let Some(from) = map.get("from").cloned() {
            map.entry("from_id".to_string()).or_insert(from);
        }
        if let Some(to) = map.get("to").cloned() {
            map.entry("to_id".to_string()).or_insert(to);
        }
    }

    Value::Object(map)
}

pub fn normalize_event_defaults(event: &mut WorkflowEvent, fallback_session_id: &str) {
    if event.id.trim().is_empty() {
        event.id = uuid::Uuid::new_v4().to_string();
    }
    if event.timestamp.trim().is_empty() {
        event.timestamp = chrono::Utc::now().to_rfc3339();
    }
    if event.session_id.trim().is_empty() {
        event.session_id = fallback_session_id.to_string();
    }
    if event.actor.trim().is_empty() {
        event.actor = "agent".to_string();
    }
}

pub async fn append_event_and_project(
    runtime: &WorkspaceRuntime,
    event: WorkflowEvent,
) -> anyhow::Result<()> {
    validate_workflow_event(&event)?;
    let mut store = runtime.workflow_store.lock().await;
    store.append(event.clone())?;
    drop(store);

    let mut index = runtime.workflow_index.lock().await;
    index.apply(&event);
    let mutations = runtime.projector.lock().await.project(&event, &index);
    drop(index);
    if !mutations.is_empty() {
        let mut graph = runtime
            .graph_store
            .load(&event.session_id)
            .unwrap_or_else(|_| SessionGraph::empty(&event.session_id));
        let broadcast_mutations = mutations.clone();
        apply_mutations(&mut graph, mutations);
        runtime.graph_store.save(&graph)?;
        for mutation in broadcast_mutations {
            let _ = runtime.graph_events.send(SessionGraphMutation {
                session_id: event.session_id.clone(),
                mutation,
            });
        }
    }
    Ok(())
}

pub fn validate_workflow_event(event: &WorkflowEvent) -> anyhow::Result<()> {
    if event.kind == WorkflowEventKind::ArtifactProduced {
        if let Some(preview) = event.payload.get("preview") {
            serde_json::from_value::<crate::workspace::ArtifactPreview>(preview.clone())
                .map_err(|error| anyhow::anyhow!("invalid artifact_produced.preview: {error}"))?;
        }
    }
    Ok(())
}

pub fn apply_mutations(graph: &mut SessionGraph, mutations: Vec<GraphMutation>) {
    for mutation in mutations {
        match mutation {
            GraphMutation::AddNode(node) => {
                graph.nodes.retain(|existing| existing.id != node.id);
                graph.nodes.push(node);
            }
            GraphMutation::AddEdge(edge) => {
                graph.edges.retain(|existing| existing.id != edge.id);
                graph.edges.push(edge);
            }
            GraphMutation::RemoveNode(node_id) => {
                graph.nodes.retain(|node| node.id != node_id);
                graph
                    .edges
                    .retain(|edge| edge.source != node_id && edge.target != node_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn workflow_event(kind: WorkflowEventKind, session_id: &str, payload: Value) -> WorkflowEvent {
        WorkflowEvent {
            id: "e1".to_string(),
            timestamp: "t".to_string(),
            session_id: session_id.to_string(),
            actor: "agent".to_string(),
            kind,
            payload,
        }
    }

    #[test]
    fn validate_workflow_event_rejects_malformed_artifact_preview() {
        let event = workflow_event(
            WorkflowEventKind::ArtifactProduced,
            "s1",
            json!({
                "artifact_id": "a1",
                "preview": {"type": "table", "columns": ["sample"], "rows": [], "total_rows": 0}
            }),
        );

        let error = validate_workflow_event(&event).unwrap_err();

        assert!(error.to_string().contains("artifact_produced.preview"));
        assert!(error.to_string().contains("kind"));
    }

    #[tokio::test]
    async fn in_process_workflow_emit_uses_tool_context_session_id() {
        let dir = tempdir().unwrap();
        let runtime = Arc::new(
            WorkspaceRuntime::open(
                "default",
                dir.path().to_path_buf(),
                dir.path().join("store"),
            )
            .unwrap(),
        );
        let tool = InProcessWorkflowEmitTool::new(runtime.clone());
        let ctx = ToolContext::new("ctx_session", "message-1");

        let result = tool
            .execute(
                json!({
                    "kind": "data_ingested",
                    "session_id": "model_supplied_wrong_session",
                    "payload": {"data_id": "samples", "summary": "samples.csv"}
                }),
                &ctx,
            )
            .await
            .unwrap();
        let body: Value = serde_json::from_str(&result.output).unwrap();

        assert_eq!(body["ok"], true);
        assert_eq!(body["event"]["session_id"], "ctx_session");
        let store = runtime.workflow_store.lock().await;
        assert_eq!(store.all()[0].session_id, "ctx_session");
    }
}
