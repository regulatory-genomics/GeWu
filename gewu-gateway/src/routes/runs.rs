use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::state::AppState;

#[derive(Deserialize)]
pub struct RunPath {
    id: String,
}

pub async fn list_runs(State(state): State<Arc<AppState>>) -> Response {
    match state.jobs.list_runs().await {
        Ok(runs) => Json(runs).into_response(),
        Err(error) => internal_error(error),
    }
}

pub async fn get_run(State(state): State<Arc<AppState>>, Path(path): Path<RunPath>) -> Response {
    match state.jobs.get_run(&path.id).await {
        Ok(Some(run)) => Json(run).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => internal_error(error),
    }
}

pub async fn get_run_logs(
    State(state): State<Arc<AppState>>,
    Path(path): Path<RunPath>,
) -> Response {
    match state.jobs.get_run(&path.id).await {
        Ok(Some(run)) => Json(serde_json::json!({
            "run_id": run.id,
            "log_path": run.log_path,
            "output": run.output,
            "error": run.error,
        }))
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => internal_error(error),
    }
}

pub async fn get_run_events(
    State(state): State<Arc<AppState>>,
    Path(path): Path<RunPath>,
) -> Response {
    match state.jobs.list_run_events(&path.id).await {
        Ok(events) => Json(events).into_response(),
        Err(error) => internal_error(error),
    }
}

fn internal_error(error: anyhow::Error) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}
