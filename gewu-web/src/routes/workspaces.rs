use crate::state::AppState;
use crate::workspace::tool::{
    append_event_and_project, normalize_event_defaults, validate_workflow_event,
};
use crate::workspace::{GraphMutation, SessionGraph, WorkflowEvent};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    pub project_root: String,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceResponse {
    pub id: String,
    pub name: String,
    pub project_root: String,
}

#[derive(Debug, Deserialize)]
pub struct GraphQuery {
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceChatRequest {
    pub content: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub selected_nodes: Vec<String>,
}

pub async fn list_workspaces(State(app): State<Arc<AppState>>) -> impl IntoResponse {
    let projects = app.list_projects().await.unwrap_or_default();
    Json(json!({
        "workspaces": projects.into_iter().map(|p| json!({
            "id": p.project_id,
            "name": p.name,
            "project_root": p.workdir,
        })).collect::<Vec<_>>()
    }))
}

pub async fn create_workspace(
    State(app): State<Arc<AppState>>,
    Json(req): Json<CreateWorkspaceRequest>,
) -> impl IntoResponse {
    match app.create_project(req.name, req.project_root).await {
        Ok(project) => (
            StatusCode::OK,
            Json(json!({ "workspace": {
                "id": project.project_id,
                "name": project.name,
                "project_root": project.workdir,
            }})),
        ),
        Err(_) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "workspace creation failed" })),
        ),
    }
}

pub async fn get_graph(
    Path(workspace_id): Path<String>,
    Query(query): Query<GraphQuery>,
    State(app): State<Arc<AppState>>,
) -> impl IntoResponse {
    let session_id = query.session_id.unwrap_or_else(|| workspace_id.clone());
    match app
        .get_or_create_workspace_runtime(&workspace_id)
        .await
        .and_then(|rt| rt.graph_store.load(&session_id))
    {
        Ok(graph) => (
            StatusCode::OK,
            Json(
                json!({ "session_id": graph.session_id, "nodes": graph.nodes, "edges": graph.edges }),
            ),
        ),
        Err(_) => (
            StatusCode::BAD_REQUEST,
            Json(
                json!({ "session_id": session_id, "nodes": [], "edges": [], "error": "graph unavailable" }),
            ),
        ),
    }
}

pub async fn delete_graph(
    Path(workspace_id): Path<String>,
    Query(query): Query<GraphQuery>,
    State(app): State<Arc<AppState>>,
) -> impl IntoResponse {
    let session_id = query.session_id.unwrap_or_else(|| workspace_id.clone());
    match app.get_or_create_workspace_runtime(&workspace_id).await {
        Ok(runtime) => {
            let graph = SessionGraph::empty(&session_id);
            match runtime.graph_store.save(&graph) {
                Ok(()) => (
                    StatusCode::OK,
                    Json(json!({"ok": true, "session_id": session_id})),
                ),
                Err(_) => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "error": "graph unavailable"})),
                ),
            }
        }
        Err(_) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "graph unavailable"})),
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum WorkflowEventsRequest {
    One(WorkflowEvent),
    Many(Vec<WorkflowEvent>),
}

impl WorkflowEventsRequest {
    fn into_events(self) -> Vec<WorkflowEvent> {
        match self {
            WorkflowEventsRequest::One(event) => vec![event],
            WorkflowEventsRequest::Many(events) => events,
        }
    }
}

fn normalize_and_validate_events(
    workspace_id: &str,
    mut events: Vec<WorkflowEvent>,
) -> Result<Vec<WorkflowEvent>, String> {
    let mut normalized = Vec::with_capacity(events.len());
    for mut event in events.drain(..) {
        normalize_event_defaults(&mut event, workspace_id);
        if let Err(error) = validate_workflow_event(&event) {
            return Err(error.to_string());
        }
        normalized.push(event);
    }
    Ok(normalized)
}

pub async fn append_workflow_event(
    Path(workspace_id): Path<String>,
    State(app): State<Arc<AppState>>,
    Json(body): Json<WorkflowEventsRequest>,
) -> impl IntoResponse {
    let runtime = match app.get_or_create_workspace_runtime(&workspace_id).await {
        Ok(runtime) => runtime,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": "workspace unavailable" })),
            );
        }
    };
    let events = match normalize_and_validate_events(&workspace_id, body.into_events()) {
        Ok(events) => events,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": error })),
            );
        }
    };
    let mut appended = Vec::with_capacity(events.len());
    for event in events {
        if let Err(error) = append_event_and_project(&runtime, event.clone()).await {
            tracing::warn!("workflow append failed: {error}");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": "workflow append failed" })),
            );
        }
        appended.push(event);
    }
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "events": appended })),
    )
}

pub async fn get_workflow_events(
    Path(workspace_id): Path<String>,
    State(app): State<Arc<AppState>>,
) -> impl IntoResponse {
    match app.get_or_create_workspace_runtime(&workspace_id).await {
        Ok(runtime) => {
            let store = runtime.workflow_store.lock().await;
            Json(json!({ "events": store.all() }))
        }
        Err(_) => Json(json!({ "events": [], "error": "workflow unavailable" })),
    }
}

pub async fn get_lineage(
    Path((workspace_id, artifact_id)): Path<(String, String)>,
    State(app): State<Arc<AppState>>,
) -> impl IntoResponse {
    match app.get_or_create_workspace_runtime(&workspace_id).await {
        Ok(runtime) => {
            let index = runtime.index().await;
            Json(json!({ "target": artifact_id, "lineage": index.lineage(&artifact_id) }))
        }
        Err(_) => {
            Json(json!({ "target": artifact_id, "lineage": [], "error": "workflow unavailable" }))
        }
    }
}

type CanvasEventStream =
    Pin<Box<dyn Stream<Item = Result<Event, std::convert::Infallible>> + Send>>;

pub async fn canvas_events(
    Path(workspace_id): Path<String>,
    Query(query): Query<GraphQuery>,
    State(app): State<Arc<AppState>>,
) -> Result<impl IntoResponse, std::convert::Infallible> {
    let requested_session_id = query.session_id;
    let stream: CanvasEventStream = match app.get_or_create_workspace_runtime(&workspace_id).await {
        Ok(runtime) => Box::pin(
            BroadcastStream::new(runtime.graph_events.subscribe()).filter_map(
                move |item| match item {
                    Ok(session_mutation)
                        if requested_session_id.as_ref().is_none_or(|session_id| {
                            session_id == &session_mutation.session_id
                        }) =>
                    {
                        Some(Ok(mutation_to_event(session_mutation.mutation)))
                    }
                    Ok(_) => None,
                    Err(_) => None,
                },
            ),
        ),
        Err(err) => {
            tracing::warn!("canvas event stream unavailable: {err}");
            Box::pin(futures::stream::once(async move {
                Ok(Event::default()
                    .event("error")
                    .data(json!({"error": "workspace unavailable"}).to_string()))
            }))
        }
    };
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

fn mutation_to_event(mutation: GraphMutation) -> Event {
    match mutation {
        GraphMutation::AddNode(node) => Event::default()
            .event("graph:node_added")
            .data(serde_json::to_string(&node).unwrap_or_default()),
        GraphMutation::AddEdge(edge) => Event::default()
            .event("graph:edge_added")
            .data(serde_json::to_string(&edge).unwrap_or_default()),
        GraphMutation::RemoveNode(node_id) => Event::default()
            .event("graph:node_removed")
            .data(json!({"node_id": node_id}).to_string()),
    }
}

pub async fn workspace_chat_send(
    Path(workspace_id): Path<String>,
    State(_app): State<Arc<AppState>>,
    Json(req): Json<WorkspaceChatRequest>,
) -> impl IntoResponse {
    let session_id = req.session_id.unwrap_or(workspace_id);
    let message = if req.selected_nodes.is_empty() {
        req.content
    } else {
        format!(
            "{}\n\nCanvas selected nodes: {}",
            req.content,
            req.selected_nodes.join(", ")
        )
    };
    Json(json!({
        "session_id": session_id,
        "message": {
            "id": uuid::Uuid::new_v4().to_string(),
            "role": "user",
            "content": message,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{GraphPosition, WorkflowEventKind};
    use serde_json::json;
    use tempfile::tempdir;

    fn event(kind: WorkflowEventKind, payload: serde_json::Value) -> WorkflowEvent {
        WorkflowEvent {
            id: "".into(),
            timestamp: "".into(),
            session_id: "".into(),
            actor: "".into(),
            kind,
            payload,
        }
    }

    #[test]
    fn normalize_and_validate_events_accepts_batch_payload() {
        let events = normalize_and_validate_events(
            "s1",
            vec![
                event(
                    WorkflowEventKind::DataIngested,
                    json!({"data_id": "samples"}),
                ),
                event(WorkflowEventKind::StepExecuted, json!({"step_id": "qc"})),
            ],
        )
        .unwrap();

        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| event.session_id == "s1"));
        assert!(events.iter().all(|event| event.actor == "agent"));
    }

    #[test]
    fn normalize_and_validate_events_rejects_preview_with_type_alias() {
        let error = normalize_and_validate_events(
            "s1",
            vec![event(
                WorkflowEventKind::ArtifactProduced,
                json!({
                    "artifact_id": "bad_preview",
                    "preview": {"type": "table", "columns": [], "rows": [], "total_rows": 0}
                }),
            )],
        )
        .unwrap_err();

        assert!(error.contains("artifact_produced.preview"));
        assert!(error.contains("kind"));
    }

    #[tokio::test]
    async fn append_event_projects_surface_into_session_graph() {
        let dir = tempdir().unwrap();
        let runtime = crate::workspace::WorkspaceRuntime::open(
            "default",
            dir.path().to_path_buf(),
            dir.path().join("store"),
        )
        .unwrap();
        append_event_and_project(&runtime, WorkflowEvent {
            id: "e1".into(),
            timestamp: "t".into(),
            session_id: "s1".into(),
            actor: "agent".into(),
            kind: WorkflowEventKind::ArtifactProduced,
            payload: json!({"artifact_id":"a1", "canvas_kind":"data_table", "preview":{"kind":"table", "columns":["sample"], "rows":[["SM30"]], "total_rows":1}}),
        }).await.unwrap();
        append_event_and_project(&runtime, WorkflowEvent {
            id: "e2".into(),
            timestamp: "t".into(),
            session_id: "s1".into(),
            actor: "agent".into(),
            kind: WorkflowEventKind::ArtifactSurfaced,
            payload: json!({"artifact_id":"a1", "canvas_node_id":"node_a1", "position": GraphPosition { x: 1.0, y: 2.0 }}),
        }).await.unwrap();
        let graph = runtime.graph_store.load("s1").unwrap();
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.nodes[0].data["payload"]["columns"][0], "sample");
        let mut rx = runtime.graph_events.subscribe();
        append_event_and_project(&runtime, WorkflowEvent {
            id: "e3".into(),
            timestamp: "t".into(),
            session_id: "s1".into(),
            actor: "agent".into(),
            kind: WorkflowEventKind::ArtifactSurfaced,
            payload: json!({"artifact_id":"a1", "canvas_node_id":"node_a1_b", "position": GraphPosition { x: 2.0, y: 3.0 }}),
        }).await.unwrap();
        let event = rx.try_recv().unwrap();
        assert_eq!(event.session_id, "s1");
    }
}
