use gewu_canvas::invocation_poller::{InvocationPoller, PendingInvocation};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};

pub struct InvocationExecutor {
    poller: InvocationPoller,
    poll_interval: Duration,
}

impl InvocationExecutor {
    pub fn new(canvas_url: &str, poll_interval_ms: u64) -> Self {
        Self {
            poller: InvocationPoller::new(canvas_url),
            poll_interval: Duration::from_millis(poll_interval_ms),
        }
    }

    #[cfg(test)]
    pub fn new_test() -> Self {
        Self::new("http://localhost:3210", 100)
    }

    pub fn build_prompt(&self, invocation: &PendingInvocation) -> String {
        format!(
            "You are being invoked as agent '{}'. Invocation ID: {}\n\n{}\n\nSelected workflow nodes: {}\n\nExecute the analysis described above. Use workflow.emit for every step and surface final artifacts to the canvas.",
            invocation.agent_id,
            invocation.invocation_id,
            invocation.briefing,
            invocation.selected_nodes.join(", "),
        )
    }

    pub async fn poll_once(&self) -> Vec<PendingInvocation> {
        match self.poller.poll().await {
            Ok(invocations) => {
                if !invocations.is_empty() {
                    info!("Received {} pending invocations", invocations.len());
                }
                invocations
            }
            Err(error) => {
                warn!("Failed to poll invocations: {error}");
                Vec::new()
            }
        }
    }

    pub async fn run_loop<F, Fut>(&self, mut execute: F)
    where
        F: FnMut(PendingInvocation, String) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        loop {
            for invocation in self.poll_once().await {
                let prompt = self.build_prompt(&invocation);
                execute(invocation, prompt).await;
            }
            sleep(self.poll_interval).await;
        }
    }
}
