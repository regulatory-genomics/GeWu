use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::scheduler::executor::execute_job_now;
use crate::scheduler::job::{CreateJobRequest, UpdateJobRequest};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct JobPath {
    id: String,
}

pub async fn list_jobs(State(state): State<Arc<AppState>>) -> Response {
    match state.jobs.list_jobs().await {
        Ok(jobs) => Json(jobs).into_response(),
        Err(error) => internal_error(error),
    }
}

pub async fn create_job(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateJobRequest>,
) -> Response {
    match state.jobs.create_job(request).await {
        Ok(job) => (StatusCode::CREATED, Json(job)).into_response(),
        Err(error) => internal_error(error),
    }
}

pub async fn get_job(State(state): State<Arc<AppState>>, Path(path): Path<JobPath>) -> Response {
    match state.jobs.get_job(&path.id).await {
        Ok(Some(job)) => Json(job).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => internal_error(error),
    }
}

pub async fn update_job(
    State(state): State<Arc<AppState>>,
    Path(path): Path<JobPath>,
    Json(request): Json<UpdateJobRequest>,
) -> Response {
    match state.jobs.update_job(&path.id, request).await {
        Ok(Some(job)) => Json(job).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => internal_error(error),
    }
}

pub async fn delete_job(State(state): State<Arc<AppState>>, Path(path): Path<JobPath>) -> Response {
    match state.jobs.delete_job(&path.id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => internal_error(error),
    }
}

pub async fn run_job_now(
    State(state): State<Arc<AppState>>,
    Path(path): Path<JobPath>,
) -> Response {
    match state.jobs.get_job(&path.id).await {
        Ok(Some(job)) => match execute_job_now(state, job).await {
            Ok(run) => Json(run).into_response(),
            Err(error) => internal_error(error),
        },
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
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
