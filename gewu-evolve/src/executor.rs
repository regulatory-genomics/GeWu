//! Executor agent for executing plans using the Agent infrastructure.

use anyhow::Result;
use futures::StreamExt;
use gewu_core::agent::Agent;
use gewu_core::types::AgentEvent;

#[derive(Debug, Clone)]
pub struct StepResult {
    pub step_id: i32,
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
    pub tool_used: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub success: bool,
    pub aborted: bool,
    pub output: String,
    pub error: Option<String>,
    pub tools_used: Vec<String>,
    pub steps_completed: i32,
    pub step_results: Vec<StepResult>,
}

pub struct Executor {
    agent: Agent,
}

impl Executor {
    pub fn new(agent: Agent) -> Self {
        Self { agent }
    }

    pub async fn execute_plan<F>(&mut self, plan: &str, mut on_event: F) -> Result<ExecutionResult>
    where
        F: FnMut(AgentEvent) + Send,
    {
        let prompt = format!(
            r#"Execute this plan using available tools.

Plan:
{}

Available tools are provided to you. Execute them as needed to complete the plan.
When the plan is complete, provide a summary of what was accomplished."#,
            plan
        );

        let mut step_results: Vec<StepResult> = Vec::new();
        let mut tools_used: Vec<String> = Vec::new();
        let mut last_output = String::new();
        let mut steps_completed = 0;
        let mut success = true;
        let mut error_msg: Option<String> = None;

        let mut stream = Box::pin(self.agent.ask(&prompt));

        while let Some(event_result) = stream.next().await {
            match event_result {
                Ok(event) => {
                    match &event {
                        AgentEvent::ToolStart { tool, .. } => {
                            tracing::info!("Tool started: {}", tool);
                        }
                        AgentEvent::ToolFinish { tool, output, .. } => {
                            tracing::info!("Tool finished: {} - {} chars", tool, output.len());
                            if !tools_used.contains(tool) {
                                tools_used.push(tool.clone());
                            }
                            steps_completed += 1;
                            step_results.push(StepResult {
                                step_id: steps_completed,
                                success: true,
                                output: output.clone(),
                                error: None,
                                tool_used: Some(tool.clone()),
                            });
                            last_output = output.clone();
                        }
                        AgentEvent::Error { error } => {
                            tracing::warn!("Agent error: {}", error);
                            success = false;
                            error_msg = Some(error.clone());
                            step_results.push(StepResult {
                                step_id: steps_completed,
                                success: false,
                                output: String::new(),
                                error: Some(error.clone()),
                                tool_used: None,
                            });
                        }
                        AgentEvent::Finish { response } => {
                            last_output = response.to_string();
                        }
                        _ => {}
                    }
                    on_event(event);
                }
                Err(e) => {
                    tracing::error!("Stream error: {}", e);
                    success = false;
                    error_msg = Some(e.to_string());
                }
            }
        }

        Ok(ExecutionResult {
            success,
            aborted: false,
            output: last_output,
            error: error_msg,
            tools_used,
            steps_completed,
            step_results,
        })
    }

}

impl Default for Executor {
    fn default() -> Self {
        Self {
            agent: Agent::builder().build().expect("Failed to build default Agent"),
        }
    }
}
