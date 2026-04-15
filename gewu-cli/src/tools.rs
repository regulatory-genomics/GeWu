use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{TaskStatus, ToolContext, ToolResult};
use gewu_storage::TaskStore;
use schemars::schema::RootSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::error::Error;
use std::sync::Arc;
use tracing::info;
use uuid::Uuid;

pub struct TaskTool {
    pub provider: Arc<dyn LlmProvider>,
    pub pool: SqlitePool,
    description: String,
}

#[derive(Deserialize)]
struct TaskArgsJson {
    description: String,
    prompt: String,
    subagent_type: String,
    #[serde(default)]
    task_id: Option<String>,
}

impl TaskTool {
    pub fn new(provider: Arc<dyn LlmProvider>, pool: SqlitePool) -> Self {
        let description = Self::generate_description();
        Self {
            provider,
            pool,
            description,
        }
    }

    fn generate_description() -> String {
        let agents = crate::registry::list_available_agents();
        let agent_list = agents
            .iter()
            .map(|a| format!("- \"{}\": {}", a.name, a.description))
            .collect::<Vec<_>>()
            .join("\n");

        include_str!("prompts/task.txt").replace("{agents}", &agent_list)
    }

    fn build_schema() -> RootSchema {
        serde_json::from_value(json!({
            "type": "object",
            "properties": {
                "description": { "type": "string", "description": "A short (3-5 words) description of the task" },
                "prompt": { "type": "string", "description": "The task for the agent to perform" },
                "subagent_type": { "type": "string", "description": "The type of specialized agent to use for this task" },
                "task_id": {
                    "type": "string",
                    "description": "This should only be set if you mean to resume a previous task (you can pass a prior task_id and the task will continue the same subagent session as before instead of creating a fresh one)"
                }
            },
            "required": ["description", "prompt", "subagent_type"]
        })).unwrap()
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> RootSchema {
        Self::build_schema()
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: TaskArgsJson = serde_json::from_value(args)?;
        let store = TaskStore::new(self.pool.clone())
            .await
            .map_err(|e| format!("TaskStore init failed: {}", e))?;

        // Determine task_id: resume existing or create new
        let task_id = match &input.task_id {
            Some(id) => id.clone(),
            None => Uuid::new_v4().to_string(),
        };

        let existing = store
            .get_task(&task_id)
            .await
            .map_err(|e| format!("Failed to look up task: {}", e))?;

        if existing.is_none() {
            store
                .create_task(&task_id, &input.subagent_type, &input.prompt)
                .await
                .map_err(|e| format!("Failed to create task: {}", e))?;
        }

        // Transition to Running and bump attempt
        store
            .update_status(&task_id, &TaskStatus::Running, None)
            .await
            .map_err(|e| format!("Failed to update status: {}", e))?;
        store
            .increment_attempt(&task_id)
            .await
            .map_err(|e| format!("Failed to increment attempt: {}", e))?;

        info!(
            "Spawning sub-agent '{}' for task: {} (task_id: {})",
            input.subagent_type, input.description, task_id
        );

        let (mut agent, _registry) = crate::registry::create_agent(
            &input.subagent_type,
            self.provider.clone(),
            self.pool.clone(),
            task_id.clone(),
            Vec::new(),
        )
        .await
        .map_err(|e| format!("Failed to create agent: {}", e))?;

        let mut stream = Box::pin(agent.ask(&input.prompt));
        let mut result = String::new();
        use futures::StreamExt;
        use gewu_core::types::AgentEvent;
        use gewu_tools::report_result::ReportResultArgs;

        let mut final_status = TaskStatus::Succeeded;
        let mut block_reason: Option<String> = None;
        let mut report_received = false;

        while let Some(event_result) = stream.next().await {
            match event_result {
                Ok(event) => match event {
                    AgentEvent::ContentDelta { delta } => {
                        result.push_str(&delta);
                    }
                    AgentEvent::Finish { response } => {
                        if !report_received {
                            result = response;
                        }
                    }
                    AgentEvent::ToolFinish { tool, output } if tool == "report_result" => {
                        report_received = true;
                        if let Ok(report) = serde_json::from_str::<ReportResultArgs>(&output) {
                            match report.status.as_str() {
                                "blocked" => {
                                    final_status = TaskStatus::Blocked;
                                    block_reason = report.block_reason.clone();
                                    result = report.block_reason.unwrap_or_default();
                                }
                                _ => {
                                    final_status = TaskStatus::Succeeded;
                                    result = report.result.unwrap_or(result);
                                }
                            }
                        }
                    }
                    AgentEvent::Error { error } => {
                        final_status = TaskStatus::Blocked;
                        block_reason = Some(error.clone());
                        result = error;
                    }
                    _ => {}
                },
                Err(e) => {
                    final_status = TaskStatus::Failed;
                    result = e.to_string();
                    break;
                }
            }
        }

        // Persist final state
        let _ = store.update_status(&task_id, &final_status, block_reason.as_deref()).await;
        if final_status == TaskStatus::Succeeded {
            let _ = store.set_result(&task_id, &result).await;
        }

        // Fetch attempt count for the envelope
        let attempt = store
            .get_task(&task_id)
            .await
            .ok()
            .flatten()
            .map(|r| r.attempt)
            .unwrap_or(0);

        let envelope = json!({
            "task_id": task_id,
            "status": final_status.as_str(),
            "attempt": attempt,
            "output": result,
        });

        Ok(ToolResult::new("task", envelope.to_string()))
    }
}
