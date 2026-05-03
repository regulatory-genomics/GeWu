//! Tools for natural-language workflow creation and invocation from any agent.
//!
//! - `compose_workflow`: delegates to the composer specialist to create a
//!   workflow from a goal description.
//! - `run_workflow`: loads and executes an existing workflow by id.

use async_trait::async_trait;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{ExecutionRunKind, SpecialistExecutionRequest, ToolContext, ToolResult};
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

use crate::execution_trace::StartTraceRun;
use crate::manifest::workflow_loader::WorkflowManifestLoader;
use crate::workflow::WorkflowCompiler;

const WORKFLOW_OUTPUT_PREVIEW_LIMIT: usize = 2_000;

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
    200
}

#[async_trait]
impl Tool for MemoryDayQueryTool {
    fn name(&self) -> &str {
        "memory_day_query"
    }

    fn description(&self) -> &str {
        "Build a compact memory_sql_query request for one local-date/source/timezone/workspace."
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
                "limit": {"type": "integer", "default": 200}
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
        let limit = input.limit.clamp(1, 500);
        let output = MemoryDayQueryOutput {
            sql: "SELECT event_id, session_id, event_kind, substr(content, 1, 1200) AS content, created_at, json_extract(metadata_json, '$.opencode.session_title') AS session_title, COALESCE(workspace_root, json_extract(metadata_json, '$.opencode.directory')) AS workspace_root FROM memory_events WHERE (agent_id = ? OR host = ? OR json_extract(metadata_json, '$.host') = ?) AND (date(created_at) = ? OR json_extract(metadata_json, '$.import.local_date') = ?) AND (? = 'local' OR COALESCE(json_extract(metadata_json, '$.import.timezone'), ?) = ?) AND (? = '' OR COALESCE(workspace_root, json_extract(metadata_json, '$.opencode.directory'), '') IN (?, '')) ORDER BY created_at LIMIT ?".into(),
            params: vec![input.source.clone(), input.source.clone(), input.source, input.date.clone(), input.date, input.timezone.clone(), input.timezone.clone(), input.timezone, input.workspace_root.clone(), input.workspace_root, limit.to_string()],
            limit,
        };
        Ok(ToolResult::new(
            "memory_day_query",
            serde_json::to_string(&output)?,
        ))
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
         then emit and persist a WorkflowManifest YAML file using the canonical \
         `entry` + map-shaped `nodes` + `finish` + `edges` schema. Returns the \
         workflow id and path on success. Use `run_workflow` to execute it afterwards."
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
        ctx: &ToolContext,
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
            workdir: ctx
                .metadata
                .get("workdir")
                .or_else(|| ctx.metadata.get("workspace_root"))
                .and_then(Value::as_str)
                .map(ToString::to_string),
            metadata_json: json!({
                "source": "compose_workflow_tool",
                "project_id": ctx.metadata.get("project_id").and_then(Value::as_str),
                "workdir": ctx.metadata.get("workdir").or_else(|| ctx.metadata.get("workspace_root")).and_then(Value::as_str),
                "workspace_root": ctx.metadata.get("workspace_root").or_else(|| ctx.metadata.get("workdir")).and_then(Value::as_str),
            }),
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
         Returns a compact JSON envelope with status, trace ids, workflow result metadata, \
         and a bounded preview. Large artifacts should be persisted by workflow nodes and \
         returned by id, not embedded in the tool output."
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

        let runtime = Arc::new(runtime);

        let parent_run_id = _ctx
            .metadata
            .get("trace_current_run_id")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let workflow_run = runtime
            .start_trace_run(StartTraceRun {
                parent_id: parent_run_id,
                session_id: Some(_ctx.session_id.clone()),
                kind: ExecutionRunKind::Workflow,
                name: input.workflow_id.clone(),
                input_json: Some(input.input.clone()),
            })
            .await
            .map_err(|e| format!("Start workflow trace run: {}", e))?;

        let compiler = WorkflowCompiler::new(Arc::clone(&runtime)).with_workflows(manifests);
        let graph = compiler
            .compile(&manifest)
            .map_err(|e| format!("Compile workflow '{}': {}", input.workflow_id, e))?;

        let mut bb = Blackboard::from_value(input.input.clone());
        if input.input.is_object() {
            bb.set_path("input", input.input);
        }
        bb.set_path("_trace.root_run_id", Value::String(workflow_run.id.clone()));
        bb.set_path(
            "_trace.current_run_id",
            Value::String(workflow_run.id.clone()),
        );
        let out = match graph.ainvoke(bb).await {
            Ok(out) => out,
            Err(e) => {
                let message = format!("Invoke workflow '{}': {}", input.workflow_id, e);
                if let Err(err) = runtime.fail_trace_run(&workflow_run.id, &message).await {
                    tracing::warn!("failed to fail workflow trace run: {}", err);
                }
                return Err(message.into());
            }
        };

        let output_value = out.as_value().clone();
        if let Err(err) = runtime
            .finish_trace_run(&workflow_run.id, Some(&output_value))
            .await
        {
            tracing::warn!("failed to finish workflow trace run: {}", err);
        }

        let output = workflow_output_envelope(&input.workflow_id, &workflow_run.id, &output_value);
        Ok(ToolResult::new("run_workflow", output))
    }
}

fn workflow_output_envelope(workflow_id: &str, run_id: &str, value: &Value) -> String {
    let errors = value.get("_errors").cloned().unwrap_or(Value::Null);
    let write_result = value.get("write_result").cloned().unwrap_or(Value::Null);
    let summary = value.get("summary").cloned().unwrap_or(Value::Null);
    let event_count = value
        .get("events")
        .and_then(extract_row_count)
        .or_else(|| value.get("event_count").and_then(Value::as_u64));

    let envelope = json!({
        "ok": errors.is_null(),
        "workflow_id": workflow_id,
        "status": if errors.is_null() { "succeeded" } else { "completed_with_errors" },
        "_trace": {
            "root_run_id": run_id,
            "current_run_id": run_id,
        },
        "event_count": event_count,
        "write_result": compact_value(write_result),
        "summary": compact_summary(summary),
        "errors": errors,
        "preview": truncate_value_preview(value, WORKFLOW_OUTPUT_PREVIEW_LIMIT),
        "note": "Workflow output is compact by design. Inspect the trace graph for full blackboard and node outputs.",
    });

    serde_json::to_string_pretty(&envelope).unwrap_or_else(|_| "{}".to_string())
}

fn extract_row_count(value: &Value) -> Option<u64> {
    match value {
        Value::Object(map) => map.get("row_count").and_then(Value::as_u64),
        Value::String(text) => parse_wrapped_json(text).ok().and_then(|parsed| {
            parsed.get("row_count").and_then(Value::as_u64).or_else(|| {
                parsed
                    .get("rows")
                    .and_then(Value::as_array)
                    .map(|rows| rows.len() as u64)
            })
        }),
        _ => None,
    }
}

fn parse_wrapped_json(text: &str) -> Result<Value, serde_json::Error> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Ok(value);
    }

    let Some(start) = text.find('{').or_else(|| text.find('[')) else {
        return serde_json::from_str(text);
    };
    serde_json::from_str(&text[start..])
}

fn compact_summary(value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            if let Some(content) = map.get_mut("content") {
                *content = Value::String(truncate_text(&value_to_text(content), 700));
            }
            Value::Object(map)
        }
        Value::String(text) => Value::String(truncate_text(&text, 700)),
        other => other,
    }
}

fn compact_value(value: Value) -> Value {
    match value {
        Value::String(text) => serde_json::from_str::<Value>(&text)
            .map(compact_value)
            .unwrap_or_else(|_| Value::String(truncate_text(&text, 700))),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let compacted = if key == "content" || key == "output" {
                        Value::String(truncate_text(&value_to_text(&value), 700))
                    } else {
                        compact_value(value)
                    };
                    (key, compacted)
                })
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.into_iter().take(20).map(compact_value).collect())
        }
        other => other,
    }
}

fn truncate_value_preview(value: &Value, limit: usize) -> String {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value_to_text(value));
    truncate_text(&text, limit)
}

fn value_to_text(value: &Value) -> String {
    value
        .as_str()
        .map(ToString::to_string)
        .unwrap_or_else(|| serde_json::to_string_pretty(value).unwrap_or_default())
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out = text.chars().take(limit).collect::<String>();
    out.push_str("…");
    out
}
