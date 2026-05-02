use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{
    ExecutionRunKind, SpecialistExecutionRequest, TaskRecord, ToolContext, ToolExecutionPolicy,
    ToolResult,
};
use gewu_storage::AsyncTaskGroupStore;
use schemars::schema::RootSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::error::Error;
use std::sync::Arc;
use tracing::info;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum TaskMode {
    Barrier,
    Async,
}

impl TaskMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Barrier => "barrier",
            Self::Async => "async",
        }
    }
}

impl Default for TaskMode {
    fn default() -> Self {
        Self::Barrier
    }
}

pub struct TaskTool {
    pub provider: Arc<dyn LlmProvider>,
    pub pool: SqlitePool,
    description: String,
}

pub struct GetTaskStatusTool {
    pub pool: SqlitePool,
}

pub struct ListChildTasksTool {
    pub pool: SqlitePool,
}

#[derive(Deserialize)]
struct TaskArgsJson {
    description: String,
    prompt: String,
    subagent_type: String,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    mode: TaskMode,
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
                },
                "mode": {
                    "type": "string",
                    "enum": ["barrier", "async"],
                    "description": "Use barrier when the parent needs the subagent result before answering. Use async to launch the subagent in the background and return a task handle immediately. Defaults to barrier."
                }
            },
            "required": ["description", "prompt", "subagent_type"]
        })).unwrap()
    }
}

fn task_record_json(record: TaskRecord) -> Value {
    json!({
        "task_id": record.task_id,
        "specialist_id": record.specialist_id,
        "prompt": record.prompt,
        "status": record.status.as_str(),
        "execution_status": record.execution_status.as_str(),
        "result_status": record.result_status.as_str(),
        "attempt": record.attempt,
        "block_reason": record.block_reason,
        "result_json": record.result_json,
        "finish_reason": record.finish_reason,
        "error": record.error,
        "parent_session_id": record.parent_session_id,
        "child_session_id": record.child_session_id,
        "root_run_id": record.root_run_id,
        "started_at": record.started_at,
        "finished_at": record.finished_at,
    })
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

    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::ParallelSafe
    }

    fn start_metadata(
        &self,
        args: &str,
        ctx: &ToolContext,
    ) -> Result<Value, Box<dyn Error + Send + Sync>> {
        let input: TaskArgsJson = serde_json::from_str(args)?;
        let task_id = input.task_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        Ok(json!({
            "task_id": task_id,
            "child_session_id": task_id,
            "specialist_id": input.subagent_type,
            "description": input.description,
            "mode": input.mode.as_str(),
            "parent_session_id": ctx.session_id,
            "parent_message_id": ctx.message_id,
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: TaskArgsJson = serde_json::from_value(args)?;
        let task_id = ctx
            .metadata
            .get("tool_start_metadata")
            .and_then(|metadata| metadata.get("task_id"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| input.task_id.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
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
        if let Some(root_run_id) = ctx
            .metadata
            .get("trace_root_run_id")
            .and_then(Value::as_str)
        {
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

        if input.mode == TaskMode::Async {
            let parent_session_id = ctx.session_id.clone();
            let parent_message_id = ctx.message_id.clone();
            let group_store = AsyncTaskGroupStore::new(self.pool.clone()).await?;
            let group = group_store
                .create_or_get_group(&parent_session_id, &parent_message_id)
                .await?;
            group_store
                .add_member(&group.group_id, &task_id, &request.session_id)
                .await?;

            let request_for_task = request.clone();
            let runtime_for_task = runtime;
            let task_id_for_output = task_id.clone();
            let specialist_id_for_output = request.specialist_id.clone();
            let group_id_for_output = group.group_id.clone();
            tokio::spawn(async move {
                if let Err(err) = runtime_for_task
                    .execute_request(&request_for_task, Vec::new())
                    .await
                {
                    tracing::warn!(
                        "background task {} failed: {}",
                        request_for_task.task_id,
                        err
                    );
                }
            });

            let output = json!({
                "task_id": task_id_for_output,
                "child_session_id": task_id,
                "specialist_id": specialist_id_for_output,
                "group_id": group_id_for_output,
                "parent_session_id": parent_session_id,
                "parent_message_id": parent_message_id,
                "mode": input.mode.as_str(),
                "execution_status": "running",
                "result_status": "pending",
                "message": "Task launched in background. Do NOT poll for status — you will be notified automatically when all background tasks complete. End your turn now."
            });
            return Ok(ToolResult::new("task", serde_json::to_string(&output)?));
        }

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

#[derive(Deserialize)]
struct GetTaskStatusArgs {
    task_id: String,
}

#[async_trait]
impl Tool for GetTaskStatusTool {
    fn name(&self) -> &str {
        "get_task_status"
    }

    fn description(&self) -> &str {
        "Get the persisted runtime/result status for one child task by task_id. Use this when users ask about progress or whether a subagent finished."
    }

    fn schema(&self) -> RootSchema {
        serde_json::from_value(json!({
            "type": "object",
            "properties": { "task_id": { "type": "string" } },
            "required": ["task_id"]
        }))
        .unwrap()
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: GetTaskStatusArgs = serde_json::from_value(args)?;
        let store = gewu_storage::TaskStore::new(self.pool.clone()).await?;
        let output = match store.get_task(&input.task_id).await? {
            Some(record) => task_record_json(record),
            None => json!({ "task_id": input.task_id, "error": "task not found" }),
        };
        Ok(ToolResult::new(
            "get_task_status",
            serde_json::to_string(&output)?,
        ))
    }
}

#[derive(Deserialize)]
struct ListChildTasksArgs {
    #[serde(default)]
    parent_session_id: Option<String>,
}

#[async_trait]
impl Tool for ListChildTasksTool {
    fn name(&self) -> &str {
        "list_child_tasks"
    }

    fn description(&self) -> &str {
        "List child tasks for a parent session, including execution_status/result_status and child_session_id. Use this instead of guessing progress."
    }

    fn schema(&self) -> RootSchema {
        serde_json::from_value(json!({
            "type": "object",
            "properties": { "parent_session_id": { "type": "string", "description": "Defaults to the current session when omitted" } }
        })).unwrap()
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: ListChildTasksArgs = serde_json::from_value(args)?;
        let parent_session_id = input
            .parent_session_id
            .unwrap_or_else(|| ctx.session_id.clone());
        let store = gewu_storage::TaskStore::new(self.pool.clone()).await?;
        let tasks = store
            .list_child_tasks(&parent_session_id)
            .await?
            .into_iter()
            .map(task_record_json)
            .collect::<Vec<_>>();
        Ok(ToolResult::new(
            "list_child_tasks",
            serde_json::to_string(&json!({
                "parent_session_id": parent_session_id,
                "tasks": tasks,
            }))?,
        ))
    }
}
