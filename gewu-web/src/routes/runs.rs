use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, Query, State},
};
use gewu_storage::ExecutionTraceStore;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct RunQuery {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
pub struct RunPath {
    pub id: String,
}

pub async fn handle_list_runs(
    State(app): State<Arc<AppState>>,
    Query(query): Query<RunQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let store = trace_store(&app).await?;
    let runs = store.root_runs_for_session(&query.session_id).await?;
    Ok(Json(serde_json::json!({ "runs": runs })))
}

pub async fn handle_get_run(
    State(app): State<Arc<AppState>>,
    Path(path): Path<RunPath>,
) -> Result<Json<serde_json::Value>, AppError> {
    let store = trace_store(&app).await?;
    let run = store.get_run(&path.id).await?.ok_or(AppError::NotFound)?;
    Ok(Json(serde_json::json!({ "run": run })))
}

pub async fn handle_get_run_children(
    State(app): State<Arc<AppState>>,
    Path(path): Path<RunPath>,
) -> Result<Json<serde_json::Value>, AppError> {
    let store = trace_store(&app).await?;
    let children = store.child_runs(&path.id).await?;
    Ok(Json(serde_json::json!({ "children": children })))
}

pub async fn handle_get_run_events(
    State(app): State<Arc<AppState>>,
    Path(path): Path<RunPath>,
) -> Result<Json<serde_json::Value>, AppError> {
    let store = trace_store(&app).await?;
    let events = store.events_for_run(&path.id).await?;
    Ok(Json(serde_json::json!({ "events": events })))
}

async fn trace_store(app: &AppState) -> anyhow::Result<ExecutionTraceStore> {
    let pool = &app.db_pool;
    ExecutionTraceStore::new(pool.clone()).await
}

#[derive(Debug)]
pub enum AppError {
    NotFound,
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
            AppError::NotFound => axum::http::StatusCode::NOT_FOUND.into_response(),
            AppError::InternalError(msg) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response(),
        }
    }
}
