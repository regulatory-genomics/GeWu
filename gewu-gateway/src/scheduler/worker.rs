use std::sync::Arc;
use std::time::Duration;

use crate::scheduler::executor::execute_job_now;
use crate::scheduler::store::now_unix;
use crate::state::AppState;

pub fn spawn_scheduler(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            if let Err(error) = tick(state.clone()).await {
                tracing::warn!("gateway scheduler tick failed: {error}");
            }
        }
    });
}

async fn tick(state: Arc<AppState>) -> anyhow::Result<()> {
    let due_jobs = state.jobs.due_jobs(now_unix()).await?;
    for job in due_jobs {
        state.jobs.advance_job(&job).await?;
        let state_for_job = state.clone();
        tokio::spawn(async move {
            if let Err(error) = execute_job_now(state_for_job, job).await {
                tracing::warn!("scheduled job execution failed: {error}");
            }
        });
    }
    Ok(())
}
