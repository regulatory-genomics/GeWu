use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{ExecutionRunKind, SpecialistExecutionRequest, ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::Deserialize;
use serde_json::{Value, json};
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
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: TaskArgsJson = serde_json::from_value(args)?;
        let task_id = match &input.task_id {
            Some(id) => id.clone(),
            None => Uuid::new_v4().to_string(),
        };
        let mut metadata = serde_json::Map::new();
        metadata.insert("description".to_string(), Value::String(input.description));
        metadata.insert(
            "parent_session_id".to_string(),
            Value::String(ctx.session_id.clone()),
        );
        metadata.insert(
            "parent_message_id".to_string(),
            Value::String(ctx.message_id.clone()),
        );
        if let Some(root_run_id) = ctx.metadata.get("trace_root_run_id").and_then(Value::as_str) {
            metadata.insert(
                "trace_root_run_id".to_string(),
                Value::String(root_run_id.to_string()),
            );
        }
        if let Some(current_run_id) = ctx
            .metadata
            .get("trace_current_run_id")
            .and_then(Value::as_str)
        {
            metadata.insert(
                "trace_current_run_id".to_string(),
                Value::String(current_run_id.to_string()),
            );
        }

        let request = SpecialistExecutionRequest {
            task_id: task_id.clone(),
            specialist_id: input.subagent_type.clone(),
            session_id: task_id.clone(),
            prompt: input.prompt.clone(),
            metadata_json: Value::Object(metadata),
        };

        info!(
            "Spawning sub-agent '{}' for task: {} (task_id: {})",
            request.specialist_id,
            request
                .metadata_json
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            task_id
        );

        let mut builder = crate::runtime::GeWuRuntime::builder()
            .provider(self.provider.clone())
            .pool(self.pool.clone())
            .register_builtin_capabilities()
            .register_builtin_specialists();

        // Load ~/.gewu/specialists/ so custom specialists (module-analyst, etc.) are available
        let home_specialists = dirs::home_dir()
            .map(|h| h.join(".gewu").join("specialists"))
            .unwrap_or_else(|| std::path::PathBuf::from(".gewu/specialists"));
        if home_specialists.is_dir() {
            builder = builder.with_manifest_dir(&home_specialists);
        }

        let project_specialists = std::path::PathBuf::from(".gewu/specialists");
        if project_specialists.is_dir() && project_specialists != home_specialists {
            builder = builder.with_manifest_dir(project_specialists);
        }

        let runtime = builder
            .build()
            .map_err(|e| format!("Failed to build runtime: {}", e))?;

        let envelope = runtime
            .execute_request(&request, Vec::new())
            .await
            .map_err(|e| format!("Failed to execute request: {}", e))?;

        let trace_store = gewu_storage::ExecutionTraceStore::new(self.pool.clone()).await?;
        let root_run = trace_store
            .runs_for_session(&task_id)
            .await?
            .into_iter()
            .find(|run| run.kind == ExecutionRunKind::Specialist);

        let mut output = serde_json::to_value(&envelope)?;
        if let Some(root_run) = root_run {
            if let Some(object) = output.as_object_mut() {
                object.insert(
                    "_trace".to_string(),
                    json!({
                        "root_run_id": root_run.id,
                        "current_run_id": root_run.id,
                        "session_id": task_id,
                        "specialist_id": request.specialist_id,
                    }),
                );
            }
        }

        Ok(ToolResult::new("task", serde_json::to_string(&output)?))
    }
}
