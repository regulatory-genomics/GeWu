use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use crate::scheduler::executor::execute_job_now;
use crate::scheduler::job::{CreateJobRequest, UpdateJobRequest};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct CommandRequest {
    #[serde(rename = "type")]
    pub command_type: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandResponse {
    pub command_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

pub async fn execute_command(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CommandRequest>,
) -> Response {
    match dispatch(state, request).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

async fn dispatch(
    state: Arc<AppState>,
    request: CommandRequest,
) -> anyhow::Result<CommandResponse> {
    let command_id = format!("cmd_{}", uuid::Uuid::new_v4());
    match request.command_type.as_str() {
        "run_job" => {
            let job_id = required_string(&request.payload, "jobId")
                .or_else(|_| required_string(&request.payload, "job_id"))?;
            let job = state
                .jobs
                .get_job(&job_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("unknown job '{job_id}'"))?;
            let run = execute_job_now(state, job).await?;
            Ok(CommandResponse {
                command_id,
                status: "accepted".to_string(),
                job_id: Some(job_id),
                run_id: Some(run.id.clone()),
                result: Some(serde_json::to_value(run)?),
            })
        }
        "create_job" => {
            let create: CreateJobRequest = serde_json::from_value(request.payload)?;
            let job = state.jobs.create_job(create).await?;
            Ok(CommandResponse {
                command_id,
                status: "completed".to_string(),
                job_id: Some(job.id.clone()),
                run_id: None,
                result: Some(serde_json::to_value(job)?),
            })
        }
        "update_job" => {
            let job_id = required_string(&request.payload, "jobId")
                .or_else(|_| required_string(&request.payload, "job_id"))?;
            let update_value = request
                .payload
                .get("update")
                .cloned()
                .unwrap_or_else(|| request.payload.clone());
            let update: UpdateJobRequest = serde_json::from_value(update_value)?;
            let job = state
                .jobs
                .update_job(&job_id, update)
                .await?
                .ok_or_else(|| anyhow::anyhow!("unknown job '{job_id}'"))?;
            Ok(CommandResponse {
                command_id,
                status: "completed".to_string(),
                job_id: Some(job.id.clone()),
                run_id: None,
                result: Some(serde_json::to_value(job)?),
            })
        }
        "delete_job" => {
            let job_id = required_string(&request.payload, "jobId")
                .or_else(|_| required_string(&request.payload, "job_id"))?;
            let deleted = state.jobs.delete_job(&job_id).await?;
            Ok(CommandResponse {
                command_id,
                status: if deleted { "completed" } else { "not_found" }.to_string(),
                job_id: Some(job_id),
                run_id: None,
                result: Some(serde_json::json!({ "deleted": deleted })),
            })
        }
        "enable_job" | "disable_job" => {
            let job_id = required_string(&request.payload, "jobId")
                .or_else(|_| required_string(&request.payload, "job_id"))?;
            let update = UpdateJobRequest {
                enabled: Some(request.command_type == "enable_job"),
                ..UpdateJobRequest::default()
            };
            let job = state
                .jobs
                .update_job(&job_id, update)
                .await?
                .ok_or_else(|| anyhow::anyhow!("unknown job '{job_id}'"))?;
            Ok(CommandResponse {
                command_id,
                status: "completed".to_string(),
                job_id: Some(job.id.clone()),
                run_id: None,
                result: Some(serde_json::to_value(job)?),
            })
        }
        other => anyhow::bail!("unsupported command type '{other}'"),
    }
}

fn required_string(payload: &Value, key: &str) -> anyhow::Result<String> {
    payload
        .get(key)
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow::anyhow!("missing string payload field '{key}'"))
}
