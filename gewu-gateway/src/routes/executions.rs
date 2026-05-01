use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use gewu_storage::ExecutionTraceStore;
use serde::Deserialize;
use std::sync::Arc;

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ExecutionQuery {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ExecutionPath {
    id: String,
}

pub async fn list_executions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ExecutionQuery>,
) -> Response {
    match trace_store(&state).await {
        Ok(store) => match store.root_runs_for_session(&query.session_id).await {
            Ok(runs) => Json(serde_json::json!({ "runs": runs })).into_response(),
            Err(error) => internal_error(error),
        },
        Err(error) => internal_error(error),
    }
}

pub async fn get_execution(
    State(state): State<Arc<AppState>>,
    Path(path): Path<ExecutionPath>,
) -> Response {
    match trace_store(&state).await {
        Ok(store) => match store.get_run(&path.id).await {
            Ok(Some(run)) => Json(serde_json::json!({ "run": run })).into_response(),
            Ok(None) => StatusCode::NOT_FOUND.into_response(),
            Err(error) => internal_error(error),
        },
        Err(error) => internal_error(error),
    }
}

pub async fn get_execution_children(
    State(state): State<Arc<AppState>>,
    Path(path): Path<ExecutionPath>,
) -> Response {
    match trace_store(&state).await {
        Ok(store) => match store.child_runs(&path.id).await {
            Ok(children) => Json(serde_json::json!({ "children": children })).into_response(),
            Err(error) => internal_error(error),
        },
        Err(error) => internal_error(error),
    }
}

pub async fn get_execution_events(
    State(state): State<Arc<AppState>>,
    Path(path): Path<ExecutionPath>,
) -> Response {
    match trace_store(&state).await {
        Ok(store) => match store.events_for_run(&path.id).await {
            Ok(events) => Json(serde_json::json!({ "events": events })).into_response(),
            Err(error) => internal_error(error),
        },
        Err(error) => internal_error(error),
    }
}

async fn trace_store(state: &AppState) -> anyhow::Result<ExecutionTraceStore> {
    ExecutionTraceStore::new(state.pool.clone()).await
}

fn internal_error(error: anyhow::Error) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}
