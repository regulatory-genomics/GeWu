use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use crate::scheduler::executor::execute_job_now;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct InspectorCommandRequest {
    #[serde(rename = "type")]
    pub command_type: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectorCommandResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn dispatch(
    state: Arc<AppState>,
    request: InspectorCommandRequest,
) -> InspectorCommandResponse {
    match request.command_type.as_str() {
        "run_job" => dispatch_run_job(state, &request.payload).await,
        "enable_job" => dispatch_toggle_job(state, &request.payload, true).await,
        "disable_job" => dispatch_toggle_job(state, &request.payload, false).await,
        "update_job" => dispatch_update_job(state, &request.payload).await,
        other => InspectorCommandResponse {
            status: "error".to_string(),
            run_id: None,
            error: Some(format!("unsupported command type '{other}'")),
        },
    }
}

async fn dispatch_run_job(state: Arc<AppState>, payload: &Value) -> InspectorCommandResponse {
    let Some(job_id) = payload
        .get("jobId")
        .or_else(|| payload.get("job_id"))
        .and_then(|v| v.as_str())
    else {
        return InspectorCommandResponse {
            status: "error".to_string(),
            run_id: None,
            error: Some("missing jobId".to_string()),
        };
    };

    let job = match state.jobs.get_job(job_id).await {
        Ok(Some(j)) => j,
        Ok(None) => {
            return InspectorCommandResponse {
                status: "error".to_string(),
                run_id: None,
                error: Some(format!("unknown job '{job_id}'")),
            };
        }
        Err(e) => {
            return InspectorCommandResponse {
                status: "error".to_string(),
                run_id: None,
                error: Some(e.to_string()),
            };
        }
    };

    match execute_job_now(state, job).await {
        Ok(run) => InspectorCommandResponse {
            status: "accepted".to_string(),
            run_id: Some(run.id),
            error: None,
        },
        Err(e) => InspectorCommandResponse {
            status: "error".to_string(),
            run_id: None,
            error: Some(e.to_string()),
        },
    }
}

async fn dispatch_toggle_job(
    state: Arc<AppState>,
    payload: &Value,
    enabled: bool,
) -> InspectorCommandResponse {
    let Some(job_id) = payload
        .get("jobId")
        .or_else(|| payload.get("job_id"))
        .and_then(|v| v.as_str())
    else {
        return InspectorCommandResponse {
            status: "error".to_string(),
            run_id: None,
            error: Some("missing jobId".to_string()),
        };
    };

    let update = crate::scheduler::job::UpdateJobRequest {
        enabled: Some(enabled),
        ..Default::default()
    };

    match state.jobs.update_job(job_id, update).await {
        Ok(Some(_)) => InspectorCommandResponse {
            status: "completed".to_string(),
            run_id: None,
            error: None,
        },
        Ok(None) => InspectorCommandResponse {
            status: "error".to_string(),
            run_id: None,
            error: Some(format!("unknown job '{job_id}'")),
        },
        Err(e) => InspectorCommandResponse {
            status: "error".to_string(),
            run_id: None,
            error: Some(e.to_string()),
        },
    }
}

async fn dispatch_update_job(state: Arc<AppState>, payload: &Value) -> InspectorCommandResponse {
    let Some(job_id) = payload
        .get("jobId")
        .or_else(|| payload.get("job_id"))
        .and_then(|v| v.as_str())
    else {
        return InspectorCommandResponse {
            status: "error".to_string(),
            run_id: None,
            error: Some("missing jobId".to_string()),
        };
    };

    let update = crate::scheduler::job::UpdateJobRequest {
        name: payload
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from),
        enabled: payload.get("enabled").and_then(|v| v.as_bool()),
        schedule: payload
            .get("schedule")
            .and_then(|v| v.as_str())
            .map(String::from),
        timezone: payload
            .get("timezone")
            .and_then(|v| v.as_str())
            .map(String::from),
        specialist_id: payload
            .get("specialistId")
            .and_then(|v| v.as_str())
            .map(String::from),
        prompt: payload
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(String::from),
        workdir: payload
            .get("workdir")
            .and_then(|v| v.as_str())
            .map(String::from),
        no_overlap: payload.get("noOverlap").and_then(|v| v.as_bool()),
        timeout_seconds: payload.get("timeoutSeconds").and_then(|v| v.as_i64()),
        next_run_at: None,
    };

    match state.jobs.update_job(job_id, update).await {
        Ok(Some(_)) => InspectorCommandResponse {
            status: "completed".to_string(),
            run_id: None,
            error: None,
        },
        Ok(None) => InspectorCommandResponse {
            status: "error".to_string(),
            run_id: None,
            error: Some(format!("unknown job '{job_id}'")),
        },
        Err(e) => InspectorCommandResponse {
            status: "error".to_string(),
            run_id: None,
            error: Some(e.to_string()),
        },
    }
}
