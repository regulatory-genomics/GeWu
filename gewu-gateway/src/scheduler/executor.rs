use anyhow::Result;
use gewu_core::types::SpecialistExecutionRequest;
use std::sync::Arc;

use crate::scheduler::job::{JobRun, RunEvent, ScheduledJob};
use crate::scheduler::store::now_unix;
use crate::state::AppState;

pub async fn execute_job_now(state: Arc<AppState>, job: ScheduledJob) -> Result<JobRun> {
    if job.no_overlap && state.jobs.has_running_run(&job.id).await? {
        anyhow::bail!("job '{}' already has a running execution", job.id);
    }
    let run = state.jobs.create_run(&job.id, &job.prompt).await?;
    let effective_workdir = job.workdir.clone();
    let request = SpecialistExecutionRequest {
        task_id: format!("gateway-job:{}:{}", job.id, run.id),
        specialist_id: job.specialist_id.clone(),
        session_id: format!("gateway-session:{}", run.id),
        prompt: job.prompt.clone(),
        workdir: effective_workdir.clone(),
        metadata_json: serde_json::json!({
            "source": "gewu-gateway",
            "job_id": job.id,
            "run_id": run.id,
            "workdir": effective_workdir,
            "workspace_root": effective_workdir,
            "timezone": job.timezone,
        }),
    };

    let result = state.runtime.execute_request(&request, Vec::new()).await;

    if let Ok(ref envelope) = result {
        let events: Vec<RunEvent> = envelope
            .events
            .iter()
            .enumerate()
            .map(|(i, evt)| RunEvent {
                id: format!("{}:evt:{}", run.id, i),
                run_id: run.id.clone(),
                event_type: evt.event_type.clone(),
                timestamp: now_unix(),
                payload: serde_json::to_string(&evt.payload_json).ok(),
            })
            .collect();
        if let Err(e) = state.jobs.insert_run_events(&events).await {
            tracing::warn!("failed to persist run events for {}: {e}", run.id);
        }
    }

    let output = result
        .as_ref()
        .ok()
        .and_then(|envelope| serde_json::to_string_pretty(envelope).ok());
    let error = result.err().map(|error| error.to_string());
    state.jobs.finish_run(&run.id, output, error).await
}
