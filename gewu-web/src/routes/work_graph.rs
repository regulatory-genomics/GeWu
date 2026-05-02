use crate::state::{AppState, SessionInfo};
use axum::{extract::State, Json};
use gewu_core::types::{
    ExecutionEvent, ExecutionRun, ExecutionRunKind, ExecutionRunStatus, TaskExecutionStatus,
    TaskRecord, TaskResultStatus,
};
use gewu_core::{
    WorkEdge, WorkEdgeKind, WorkEvent, WorkEventKind, WorkGraph, WorkNode, WorkNodeKind, WorkStatus,
};
use gewu_storage::{ExecutionTraceStore, TaskStore};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Debug, Serialize)]
pub struct WorkGraphResponse {
    pub graph: WorkGraph,
}

pub async fn handle_get_work_graph(
    State(app): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<WorkGraphResponse>, AppError> {
    let (session_id, session) = resolve_session(&app, &id).await;
    let pool_guard = app.db_pool.lock().await;
    let pool = pool_guard
        .as_ref()
        .ok_or_else(|| AppError::InternalError("Database is not initialized".to_string()))?
        .clone();
    drop(pool_guard);

    let task_store = TaskStore::new(pool.clone()).await?;
    let trace_store = ExecutionTraceStore::new(pool).await?;
    let tasks = task_store.list_child_tasks(&session_id).await?;
    let runs = trace_store.runs_for_session(&session_id).await?;

    let mut graph = WorkGraph {
        root_node_id: session_node_id(&session_id),
        nodes: vec![session_node(&session_id, session.as_ref())],
        edges: Vec::new(),
        events: Vec::new(),
        artifacts: Vec::new(),
    };

    if let Some(turn) = app.get_active_turn(&session_id).await {
        append_active_turn(&mut graph, turn);
    }

    for task in tasks {
        append_task(&mut graph, task);
    }

    for run in runs {
        let events = trace_store
            .events_for_run(&run.id)
            .await
            .unwrap_or_default();
        append_run(&mut graph, run, events);
    }

    graph.events.sort_by_key(|event| event.created_at);
    Ok(Json(WorkGraphResponse { graph }))
}

async fn resolve_session(app: &AppState, id: &str) -> (String, Option<SessionInfo>) {
    let resolved_id = app
        .resolve_session_id(id)
        .await
        .unwrap_or_else(|_| id.to_string());
    let session = app.get_session(&resolved_id).await;
    let id_out = session
        .as_ref()
        .map(|s| s.id.clone())
        .unwrap_or(resolved_id);
    (id_out, session)
}

fn append_active_turn(graph: &mut WorkGraph, turn: crate::state::ActiveTurnInfo) {
    let node_id = format!("turn:{}:{}", turn.session_id, turn.started_at);
    let mut node = WorkNode::new(
        node_id.clone(),
        WorkNodeKind::AssistantTurn,
        "Active assistant turn",
        WorkStatus::Running,
    );
    node.session_id = Some(turn.session_id.clone());
    node.started_at = Some(turn.started_at);
    node.summary = Some(format!("{} streamed events", turn.event_count));
    node.metadata = json!({
        "source": "active_turn",
        "last_event_at": turn.last_event_at,
        "event_count": turn.event_count,
    });
    graph.nodes.push(node);
    graph.edges.push(WorkEdge::new(
        graph.root_node_id.clone(),
        node_id.clone(),
        WorkEdgeKind::Contains,
    ));
    graph.events.push(WorkEvent {
        id: format!("event:{node_id}:running"),
        node_id,
        kind: WorkEventKind::Started,
        title: "Assistant turn is running".to_string(),
        summary: Some(format!("last event at {}", turn.last_event_at)),
        created_at: turn.started_at,
        payload: Value::Null,
    });
}

fn append_task(graph: &mut WorkGraph, task: TaskRecord) {
    let node_id = task_node_id(&task.task_id);
    let child_session_id = task.child_session_id.clone();
    let root_run_id = task.root_run_id.clone();
    let mut node = WorkNode::new(
        node_id.clone(),
        WorkNodeKind::Task,
        task_title(&task),
        task_work_status(&task),
    );
    node.summary = Some(task.prompt.clone());
    node.started_at = task.started_at;
    node.finished_at = task.finished_at;
    node.session_id = child_session_id.clone();
    node.task_id = Some(task.task_id.clone());
    node.trace_run_id = root_run_id.clone();
    node.output = task
        .result_json
        .as_deref()
        .and_then(|value| serde_json::from_str(value).ok());
    node.error = task.error.clone();
    node.metadata = json!({
        "specialist_id": task.specialist_id,
        "status": task.status.as_str(),
        "execution_status": task.execution_status.as_str(),
        "result_status": task.result_status.as_str(),
        "attempt": task.attempt,
        "finish_reason": task.finish_reason,
        "block_reason": task.block_reason,
    });
    graph.nodes.push(node);
    graph.edges.push(WorkEdge::new(
        graph.root_node_id.clone(),
        node_id.clone(),
        WorkEdgeKind::Spawns,
    ));

    if let Some(child_session_id) = child_session_id {
        let child_node_id = session_node_id(&child_session_id);
        if !graph.nodes.iter().any(|node| node.id == child_node_id) {
            let mut child = WorkNode::new(
                child_node_id.clone(),
                WorkNodeKind::Subagent,
                format!("Subagent session {}", short_id(&child_session_id)),
                task_work_status(&task),
            );
            child.session_id = Some(child_session_id);
            child.task_id = Some(task.task_id.clone());
            child.trace_run_id = root_run_id.clone();
            graph.nodes.push(child);
        }
        graph.edges.push(WorkEdge::new(
            node_id.clone(),
            child_node_id,
            WorkEdgeKind::SessionOf,
        ));
    }

    if let Some(root_run_id) = root_run_id {
        graph.edges.push(WorkEdge::new(
            node_id.clone(),
            run_node_id(&root_run_id),
            WorkEdgeKind::TraceOf,
        ));
    }

    if let Some(started_at) = task.started_at {
        graph.events.push(WorkEvent {
            id: format!("event:task:{}:started", task.task_id),
            node_id: node_id.clone(),
            kind: WorkEventKind::Started,
            title: format!("Started task {}", short_id(&task.task_id)),
            summary: Some(task.prompt),
            created_at: started_at,
            payload: Value::Null,
        });
    }

    if let Some(finished_at) = task.finished_at {
        graph.events.push(WorkEvent {
            id: format!("event:task:{}:finished", task.task_id),
            node_id,
            kind: finish_event_kind(&task.execution_status),
            title: format!("Finished task {}", short_id(&task.task_id)),
            summary: task.error,
            created_at: finished_at,
            payload: Value::Null,
        });
    }
}

fn append_run(graph: &mut WorkGraph, run: ExecutionRun, events: Vec<ExecutionEvent>) {
    let node_id = run_node_id(&run.id);
    let run_kind = run.kind.clone();
    let run_name = run.name.clone();
    let input_json = run.input_json.clone();
    let mut node = WorkNode::new(
        node_id.clone(),
        run_node_kind(&run_kind),
        run.name.clone(),
        run_work_status(&run.status),
    );
    node.started_at = Some(run.started_at);
    node.finished_at = run.finished_at;
    node.session_id = run.session_id.clone();
    node.trace_run_id = Some(run.id.clone());
    node.input = run.input_json.clone();
    node.output = run.output_json.clone();
    node.error = run.error.clone();
    node.metadata = json!({
        "run_kind": run.kind.as_str(),
        "run_status": run.status.as_str(),
    });
    graph.nodes.push(node);

    let parent = run
        .parent_id
        .as_ref()
        .map(|id| run_node_id(id))
        .unwrap_or_else(|| graph.root_node_id.clone());
    graph.edges.push(WorkEdge::new(
        parent,
        node_id.clone(),
        WorkEdgeKind::Contains,
    ));

    graph.events.push(WorkEvent {
        id: format!("event:run:{}:started", run.id),
        node_id: node_id.clone(),
        kind: WorkEventKind::Started,
        title: format!("Started {}", run.name),
        summary: Some(run.kind.as_str().to_string()),
        created_at: run.started_at,
        payload: Value::Null,
    });

    if let Some(finished_at) = run.finished_at {
        graph.events.push(WorkEvent {
            id: format!("event:run:{}:finished", run.id),
            node_id: node_id.clone(),
            kind: run_finish_event_kind(&run.status),
            title: format!("Finished {}", run.name),
            summary: run.error,
            created_at: finished_at,
            payload: run.output_json.unwrap_or(Value::Null),
        });
    }

    for event in events {
        graph.events.push(WorkEvent {
            id: format!("event:{}", event.id),
            node_id: node_id.clone(),
            kind: WorkEventKind::TraceEvent,
            title: event.event_type.clone(),
            summary: None,
            created_at: event.created_at,
            payload: event.payload_json,
        });
    }

    if run_kind == ExecutionRunKind::Tool && run_name == "todowrite" {
        append_todo_nodes(graph, &node_id, run.started_at, input_json.as_ref());
    }
}

fn append_todo_nodes(
    graph: &mut WorkGraph,
    source_node_id: &str,
    created_at: i64,
    input_json: Option<&Value>,
) {
    let Some(todos) = input_json
        .and_then(|value| value.get("todos"))
        .and_then(Value::as_array)
    else {
        return;
    };

    for (index, todo) in todos.iter().enumerate() {
        let todo_id = todo
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| (index + 1).to_string());
        let content = todo
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("Untitled todo")
            .to_string();
        let raw_status = todo
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending")
            .to_string();
        let priority = todo.get("priority").cloned().unwrap_or(Value::Null);
        let node_id = format!("todo:{source_node_id}:{todo_id}");
        let mut node = WorkNode::new(
            node_id.clone(),
            WorkNodeKind::Todo,
            content.clone(),
            todo_work_status(&raw_status),
        );
        node.summary = Some(content.clone());
        node.started_at = Some(created_at);
        node.metadata = json!({
            "todo_id": todo_id,
            "todo_status": raw_status,
            "priority": priority,
            "source": "todowrite"
        });
        graph.nodes.push(node);
        graph.edges.push(WorkEdge::new(
            graph.root_node_id.clone(),
            node_id.clone(),
            WorkEdgeKind::Contains,
        ));
        graph.edges.push(WorkEdge::new(
            source_node_id.to_string(),
            node_id.clone(),
            WorkEdgeKind::Produces,
        ));
        graph.events.push(WorkEvent {
            id: format!("event:{node_id}:created"),
            node_id,
            kind: WorkEventKind::Created,
            title: format!("Todo: {content}"),
            summary: Some(format!("status: {raw_status}")),
            created_at,
            payload: todo.clone(),
        });
    }
}

fn session_node(id: &str, session: Option<&SessionInfo>) -> WorkNode {
    let mut node = WorkNode::new(
        session_node_id(id),
        WorkNodeKind::Session,
        session
            .map(|session| format!("{} session", session.agent_type))
            .unwrap_or_else(|| format!("Session {}", short_id(id))),
        WorkStatus::Running,
    );
    node.session_id = Some(id.to_string());
    node.started_at = session.map(|session| session.created_at);
    node
}

fn task_title(task: &TaskRecord) -> String {
    let first_line = task.prompt.lines().next().unwrap_or_default().trim();
    if first_line.is_empty() {
        format!("Task {}", short_id(&task.task_id))
    } else {
        first_line.chars().take(80).collect()
    }
}

fn task_work_status(task: &TaskRecord) -> WorkStatus {
    match task.execution_status {
        TaskExecutionStatus::Pending => WorkStatus::Pending,
        TaskExecutionStatus::Running => WorkStatus::Running,
        TaskExecutionStatus::Completed => match task.result_status {
            TaskResultStatus::Accepted => WorkStatus::Succeeded,
            TaskResultStatus::Pending | TaskResultStatus::NeedsReview => WorkStatus::NeedsReview,
            TaskResultStatus::Empty | TaskResultStatus::Incomplete | TaskResultStatus::Invalid => {
                WorkStatus::NeedsReview
            }
        },
        TaskExecutionStatus::Failed => match task.result_status {
            TaskResultStatus::Invalid
            | TaskResultStatus::Incomplete
            | TaskResultStatus::NeedsReview
            | TaskResultStatus::Empty => WorkStatus::NeedsReview,
            TaskResultStatus::Accepted | TaskResultStatus::Pending => WorkStatus::Failed,
        },
        TaskExecutionStatus::Timeout | TaskExecutionStatus::Interrupted => WorkStatus::Failed,
        TaskExecutionStatus::Canceled => WorkStatus::Canceled,
    }
}

fn run_work_status(status: &ExecutionRunStatus) -> WorkStatus {
    match status {
        ExecutionRunStatus::Running => WorkStatus::Running,
        ExecutionRunStatus::Succeeded => WorkStatus::Succeeded,
        ExecutionRunStatus::Failed => WorkStatus::Failed,
        ExecutionRunStatus::Cancelled => WorkStatus::Canceled,
        ExecutionRunStatus::Skipped => WorkStatus::Skipped,
    }
}

fn todo_work_status(status: &str) -> WorkStatus {
    match status {
        "completed" | "done" | "succeeded" => WorkStatus::Succeeded,
        "in_progress" | "running" => WorkStatus::Running,
        "failed" => WorkStatus::Failed,
        "cancelled" | "canceled" => WorkStatus::Canceled,
        "blocked" => WorkStatus::Blocked,
        _ => WorkStatus::Pending,
    }
}

fn run_node_kind(kind: &ExecutionRunKind) -> WorkNodeKind {
    match kind {
        ExecutionRunKind::ChatMessage => WorkNodeKind::AssistantTurn,
        ExecutionRunKind::Tool => WorkNodeKind::Tool,
        ExecutionRunKind::Workflow => WorkNodeKind::Workflow,
        ExecutionRunKind::WorkflowNode => WorkNodeKind::WorkflowNode,
        ExecutionRunKind::Specialist => WorkNodeKind::Subagent,
    }
}

fn finish_event_kind(status: &TaskExecutionStatus) -> WorkEventKind {
    match status {
        TaskExecutionStatus::Canceled => WorkEventKind::Canceled,
        TaskExecutionStatus::Failed
        | TaskExecutionStatus::Timeout
        | TaskExecutionStatus::Interrupted => WorkEventKind::Failed,
        _ => WorkEventKind::Finished,
    }
}

fn run_finish_event_kind(status: &ExecutionRunStatus) -> WorkEventKind {
    match status {
        ExecutionRunStatus::Failed => WorkEventKind::Failed,
        ExecutionRunStatus::Cancelled => WorkEventKind::Canceled,
        _ => WorkEventKind::Finished,
    }
}

fn session_node_id(id: &str) -> String {
    format!("session:{id}")
}

fn task_node_id(id: &str) -> String {
    format!("task:{id}")
}

fn run_node_id(id: &str) -> String {
    format!("run:{id}")
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[derive(Debug)]
pub enum AppError {
    InternalError(String),
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::InternalError(e.to_string())
    }
}

impl axum::response::IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        match self {
            AppError::InternalError(msg) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_core::types::TaskStatus;

    #[test]
    fn maps_accepted_completed_task_to_succeeded() {
        let task = TaskRecord {
            task_id: "task_a".into(),
            specialist_id: "general".into(),
            prompt: "Inspect module".into(),
            status: TaskStatus::Succeeded,
            execution_status: TaskExecutionStatus::Completed,
            result_status: TaskResultStatus::Accepted,
            attempt: 1,
            block_reason: None,
            result_json: None,
            finish_reason: None,
            error: None,
            parent_session_id: Some("parent".into()),
            child_session_id: Some("child".into()),
            root_run_id: Some("run_a".into()),
            started_at: Some(10),
            finished_at: Some(20),
        };

        assert_eq!(task_work_status(&task), WorkStatus::Succeeded);
        assert_eq!(task_title(&task), "Inspect module");
    }

    #[test]
    fn maps_completed_but_unreviewed_task_to_needs_review() {
        let mut task = TaskRecord {
            task_id: "task_b".into(),
            specialist_id: "general".into(),
            prompt: String::new(),
            status: TaskStatus::Succeeded,
            execution_status: TaskExecutionStatus::Completed,
            result_status: TaskResultStatus::Incomplete,
            attempt: 1,
            block_reason: None,
            result_json: None,
            finish_reason: None,
            error: None,
            parent_session_id: None,
            child_session_id: None,
            root_run_id: None,
            started_at: None,
            finished_at: None,
        };

        assert_eq!(task_work_status(&task), WorkStatus::NeedsReview);
        task.execution_status = TaskExecutionStatus::Failed;
        task.result_status = TaskResultStatus::Accepted;
        assert_eq!(task_work_status(&task), WorkStatus::Failed);
    }

    #[test]
    fn maps_completed_invalid_task_to_needs_review_not_failed() {
        let task = TaskRecord {
            task_id: "task_c".into(),
            specialist_id: "general".into(),
            prompt: String::new(),
            status: TaskStatus::Blocked,
            execution_status: TaskExecutionStatus::Completed,
            result_status: TaskResultStatus::Invalid,
            attempt: 1,
            block_reason: Some("tool protocol error".into()),
            result_json: None,
            finish_reason: Some("stop".into()),
            error: Some("tool protocol error".into()),
            parent_session_id: None,
            child_session_id: None,
            root_run_id: None,
            started_at: None,
            finished_at: None,
        };

        assert_eq!(task_work_status(&task), WorkStatus::NeedsReview);
    }

    #[test]
    fn maps_legacy_failed_invalid_task_to_needs_review() {
        let task = TaskRecord {
            task_id: "task_d".into(),
            specialist_id: "general".into(),
            prompt: String::new(),
            status: TaskStatus::Failed,
            execution_status: TaskExecutionStatus::Failed,
            result_status: TaskResultStatus::Invalid,
            attempt: 1,
            block_reason: None,
            result_json: None,
            finish_reason: Some("error".into()),
            error: Some("API tool protocol error".into()),
            parent_session_id: None,
            child_session_id: None,
            root_run_id: None,
            started_at: None,
            finished_at: None,
        };

        assert_eq!(task_work_status(&task), WorkStatus::NeedsReview);
    }
}
