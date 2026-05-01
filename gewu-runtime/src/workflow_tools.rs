//! Tools for natural-language workflow creation and invocation from any agent.
//!
//! - `compose_workflow`: delegates to the composer specialist to create a
//!   workflow from a goal description.
//! - `run_workflow`: loads and executes an existing workflow by id.

use async_trait::async_trait;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{SpecialistExecutionRequest, ToolContext, ToolResult};
use gewu_graph::Blackboard;
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use uuid::Uuid;

use crate::manifest::workflow_loader::WorkflowManifestLoader;
use crate::workflow::WorkflowCompiler;

// ---------------------------------------------------------------------------
// compose_workflow tool
// ---------------------------------------------------------------------------

pub struct MemoryDayQueryTool;

#[derive(Debug, Deserialize)]
struct MemoryDayQueryArgs {
    date: String,
    timezone: String,
    workspace_root: String,
    #[serde(default = "default_source")]
    source: String,
    #[serde(default = "default_day_query_limit")]
    limit: usize,
}

#[derive(Debug, Serialize)]
struct MemoryDayQueryOutput {
    sql: String,
    params: Vec<String>,
    limit: usize,
}

fn default_source() -> String {
    "opencode".into()
}

fn default_day_query_limit() -> usize {
    2000
}

#[async_trait]
impl Tool for MemoryDayQueryTool {
    fn name(&self) -> &str {
        "memory_day_query"
    }

    fn description(&self) -> &str {
        "Build a memory_sql_query request for one local-date/source/timezone/workspace."
    }

    fn schema(&self) -> RootSchema {
        serde_json::from_value(json!({
            "type": "object",
            "required": ["date", "timezone", "workspace_root"],
            "properties": {
                "date": {"type": "string"},
                "timezone": {"type": "string"},
                "workspace_root": {"type": "string"},
                "source": {"type": "string", "default": "opencode"},
                "limit": {"type": "integer", "default": 2000}
            }
        }))
        .unwrap()
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: MemoryDayQueryArgs = serde_json::from_value(args)?;
        let limit = input.limit.clamp(1, 5000);
        let output = MemoryDayQueryOutput {
            sql: "SELECT event_id, session_id, event_kind, content, created_at, json_extract(metadata_json, '$.opencode.session_title') AS session_title, json_extract(metadata_json, '$.opencode.directory') AS workspace_root FROM memory_events WHERE json_extract(metadata_json, '$.host') = ? AND json_extract(metadata_json, '$.import.local_date') = ? AND json_extract(metadata_json, '$.import.timezone') = ? AND json_extract(metadata_json, '$.opencode.directory') = ? ORDER BY created_at LIMIT ?".into(),
            params: vec![input.source, input.date, input.timezone, input.workspace_root, limit.to_string()],
            limit,
        };
        Ok(ToolResult::new("memory_day_query", serde_json::to_string(&output)?))
    }
}

// ---------------------------------------------------------------------------
// compose_workflow tool
// ---------------------------------------------------------------------------

pub struct ComposeWorkflowTool {
    pub provider: Arc<dyn LlmProvider>,
    pub pool: SqlitePool,
}

#[derive(Deserialize)]
struct ComposeArgs {
    goal: String,
    #[serde(default)]
    id: Option<String>,
}

impl ComposeWorkflowTool {
    pub fn new(provider: Arc<dyn LlmProvider>, pool: SqlitePool) -> Self {
        Self { provider, pool }
    }
}

#[async_trait]
impl Tool for ComposeWorkflowTool {
    fn name(&self) -> &str {
        "compose_workflow"
    }

    fn description(&self) -> &str {
        "Create a new reusable workflow from a natural-language goal description. \
         The composer specialist will inspect available specialists and capabilities, \
         then emit and persist a WorkflowManifest YAML file. Returns the workflow id \
         and path on success. Use `run_workflow` to execute it afterwards."
    }

    fn schema(&self) -> RootSchema {
        serde_json::from_value(json!({
            "type": "object",
            "properties": {
                "goal": {
                    "type": "string",
                    "description": "Natural-language description of what the workflow should accomplish. \
                                    Be specific about which specialists/tools to use, the data flow, \
                                    and the desired output structure."
                },
                "id": {
                    "type": "string",
                    "description": "Optional workflow id. If omitted, the composer will choose one."
                }
            },
            "required": ["goal"]
        }))
        .unwrap()
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: ComposeArgs = serde_json::from_value(args)?;

        let mut prompt = input.goal.clone();
        if let Some(ref id) = input.id {
            prompt = format!("{}. The workflow id must be '{}'.", prompt, id);
        }

        let task_id = Uuid::new_v4().to_string();
        let request = SpecialistExecutionRequest {
            task_id: task_id.clone(),
            specialist_id: "composer".to_string(),
            session_id: task_id.clone(),
            prompt,
            metadata_json: json!({ "source": "compose_workflow_tool" }),
        };

        info!(
            "compose_workflow: delegating to composer specialist (task_id: {})",
            task_id
        );

        let runtime = crate::runtime::GeWuRuntime::builder()
            .provider(self.provider.clone())
            .pool(self.pool.clone())
            .register_builtin_capabilities()
            .register_builtin_specialists()
            .with_manifest_dir(crate::composer::default_specialists_dir())
            .build()
            .map_err(|e| format!("Failed to build runtime: {}", e))?;

        let envelope = runtime
            .execute_request(&request, Vec::new())
            .await
            .map_err(|e| format!("Composer failed: {}", e))?;

        Ok(ToolResult::new(
            "compose_workflow",
            serde_json::to_string(&envelope)?,
        ))
    }
}

// ---------------------------------------------------------------------------
// run_workflow tool
// ---------------------------------------------------------------------------

pub struct RunWorkflowTool {
    pub provider: Arc<dyn LlmProvider>,
    pub pool: SqlitePool,
    pub workflow_dirs: Vec<PathBuf>,
    pub skill_dirs: Vec<PathBuf>,
    pub runtime_plugin_dirs: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct RunArgs {
    workflow_id: String,
    #[serde(default)]
    input: Value,
}

impl RunWorkflowTool {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        pool: SqlitePool,
        workflow_dirs: Vec<PathBuf>,
    ) -> Self {
        Self {
            provider,
            pool,
            workflow_dirs,
            skill_dirs: Vec::new(),
            runtime_plugin_dirs: Vec::new(),
        }
    }

    pub fn with_skill_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.skill_dirs = dirs;
        self
    }

    pub fn with_runtime_plugin_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.runtime_plugin_dirs = dirs;
        self
    }

    fn default_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(h) = dirs::home_dir() {
            dirs.push(h.join(".gewu").join("workflows"));
        }
        dirs.push(PathBuf::from(".gewu/workflows"));
        dirs
    }
}

#[async_trait]
impl Tool for RunWorkflowTool {
    fn name(&self) -> &str {
        "run_workflow"
    }

    fn description(&self) -> &str {
        "Execute an existing workflow by id. The workflow must have been previously \
         created (e.g. via compose_workflow or manually placed in ~/.gewu/workflows/). \
         Returns the full output blackboard as JSON."
    }

    fn schema(&self) -> RootSchema {
        serde_json::from_value(json!({
            "type": "object",
            "properties": {
                "workflow_id": {
                    "type": "string",
                    "description": "The id of the workflow to run (as shown by --list-workflows)"
                },
                "input": {
                    "type": "object",
                    "description": "JSON input to feed into the workflow's blackboard. \
                                    Nodes read from this via their `input: $.path` declarations."
                }
            },
            "required": ["workflow_id"]
        }))
        .unwrap()
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: RunArgs = serde_json::from_value(args)?;

        let dirs = if self.workflow_dirs.is_empty() {
            Self::default_dirs()
        } else {
            self.workflow_dirs.clone()
        };

        let mut loader = WorkflowManifestLoader::new();
        for d in &dirs {
            loader.add_dir(d);
        }
        let loaded = loader
            .load()
            .map_err(|e| format!("load workflows: {}", e))?;
        let manifests: std::collections::HashMap<String, _> =
            loaded.into_iter().map(|(_, m)| (m.id.clone(), m)).collect();

        let manifest = manifests
            .get(&input.workflow_id)
            .ok_or_else(|| {
                let available: Vec<&String> = manifests.keys().collect();
                format!(
                    "Workflow '{}' not found. Available: {:?}",
                    input.workflow_id, available
                )
            })?
            .clone();

        info!(
            "run_workflow: compiling and invoking '{}'",
            input.workflow_id
        );

        let runtime = crate::runtime::GeWuRuntime::builder()
            .provider(self.provider.clone())
            .pool(self.pool.clone())
            .register_builtin_capabilities()
            .register_builtin_specialists()
            .with_manifest_dir(crate::composer::default_specialists_dir())
            .with_runtime_plugin_dirs(self.runtime_plugin_dirs.clone())
            .with_skill_dirs(self.skill_dirs.clone())
            .with_workflow_dirs(dirs.clone())
            .build()
            .map_err(|e| format!("Failed to build runtime: {}", e))?;

        let compiler = WorkflowCompiler::new(Arc::new(runtime)).with_workflows(manifests);
        let graph = compiler
            .compile(&manifest)
            .map_err(|e| format!("Compile workflow '{}': {}", input.workflow_id, e))?;

        let mut bb = Blackboard::from_value(input.input.clone());
        if input.input.is_object() {
            bb.set_path("input", input.input);
        }
        let out = graph
            .ainvoke(bb)
            .await
            .map_err(|e| format!("Invoke workflow '{}': {}", input.workflow_id, e))?;

        let output =
            serde_json::to_string_pretty(out.as_value()).unwrap_or_else(|_| "{}".to_string());

        Ok(ToolResult::new("run_workflow", output))
    }
}
