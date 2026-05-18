use crate::state::AppState;
use crate::workspace::tool::{append_event_and_project, normalize_event_defaults};
use crate::workspace::{GraphMutation, SessionGraph, WorkflowEvent};
use axum::{
    extract::{Path, Query, State},
    response::{sse::{Event, Sse}, IntoResponse},
    Json,
};
use futures::Stream;
use std::pin::Pin;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

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
        Ok(project) => Json(json!({ "workspace": {
            "id": project.project_id,
            "name": project.name,
            "project_root": project.workdir,
        }})),
        Err(err) => Json(json!({ "error": err.to_string() })),
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
        Ok(graph) => Json(json!({ "session_id": graph.session_id, "nodes": graph.nodes, "edges": graph.edges })),
        Err(err) => Json(json!({ "session_id": session_id, "nodes": [], "edges": [], "error": err.to_string() })),
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
                Ok(()) => Json(json!({"ok": true, "session_id": session_id})),
                Err(err) => Json(json!({"ok": false, "error": err.to_string()})),
            }
        }
        Err(err) => Json(json!({"ok": false, "error": err.to_string()})),
    }
}

pub async fn append_workflow_event(
    Path(workspace_id): Path<String>,
    State(app): State<Arc<AppState>>,
    Json(mut event): Json<WorkflowEvent>,
) -> impl IntoResponse {
    let runtime = match app.get_or_create_workspace_runtime(&workspace_id).await {
        Ok(runtime) => runtime,
        Err(err) => return Json(json!({ "ok": false, "error": err.to_string() })),
    };
    normalize_event_defaults(&mut event, &workspace_id);
    match append_event_and_project(&runtime, event.clone()).await {
        Ok(()) => Json(json!({ "ok": true, "event": event })),
        Err(err) => Json(json!({ "ok": false, "error": err.to_string() })),
    }
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
        Err(err) => Json(json!({ "events": [], "error": err.to_string() })),
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
        Err(err) => Json(json!({ "target": artifact_id, "lineage": [], "error": err.to_string() })),
    }
}

type CanvasEventStream = Pin<Box<dyn Stream<Item = Result<Event, std::convert::Infallible>> + Send>>;

pub async fn canvas_events(
    Path(workspace_id): Path<String>,
    State(app): State<Arc<AppState>>,
) -> Result<Sse<CanvasEventStream>, std::convert::Infallible> {
    let stream: CanvasEventStream = match app.get_or_create_workspace_runtime(&workspace_id).await {
        Ok(runtime) => Box::pin(
            BroadcastStream::new(runtime.graph_events.subscribe()).filter_map(|item| match item {
                Ok(mutation) => Some(Ok(mutation_to_event(mutation))),
                Err(_) => None,
            }),
        ),
        Err(err) => Box::pin(futures::stream::once(async move {
            Ok(Event::default()
                .event("error")
                .data(json!({"error": err.to_string()}).to_string()))
        })),
    };
    Ok(Sse::new(stream))
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
        format!("{}\n\nCanvas selected nodes: {}", req.content, req.selected_nodes.join(", "))
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
    }
}
