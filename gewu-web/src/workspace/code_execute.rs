use crate::workspace::tool::append_event_and_project;
use crate::workspace::{WorkflowEvent, WorkflowEventKind, WorkspaceRuntime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::process::Stdio;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::{Duration, timeout};

static PYTHON_SANDBOX_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonResult {
    pub stdout: String,
    pub stderr: String,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub timed_out: bool,
}

pub struct PythonSandbox {
    timeout_secs: u64,
}

impl PythonSandbox {
    pub fn new(timeout_secs: u64) -> Self {
        Self { timeout_secs }
    }

    pub async fn execute(&self, code: &str) -> Result<PythonResult> {
        let _permit = PYTHON_SANDBOX_SEMAPHORE
            .get_or_init(|| Semaphore::new(1))
            .acquire()
            .await
            .context("python sandbox semaphore closed")?;
        let wrapper = Self::wrapper_code(code)?;
        let child = Command::new(micromamba_command())
            .args(["run", "-n", "py311", "python3", "-c", &wrapper])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to spawn micromamba py311 python3")?;

        match timeout(Duration::from_secs(self.timeout_secs), child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let raw_stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                if !output.status.success() {
                    let error = if stderr.trim().is_empty() {
                        format!("python exited with status {}", output.status)
                    } else {
                        stderr.clone()
                    };
                    return Ok(PythonResult {
                        stdout: raw_stdout,
                        stderr,
                        result: None,
                        error: Some(error),
                        timed_out: false,
                    });
                }

                let parsed = match serde_json::from_str::<Value>(raw_stdout.trim()) {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        return Ok(PythonResult {
                            stdout: raw_stdout,
                            stderr,
                            result: None,
                            error: Some(format!("failed to parse Python output as JSON: {error}")),
                            timed_out: false,
                        });
                    }
                };

                let captured_stdout = parsed
                    .get("stdout")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let error = parsed
                    .get("error")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let result = if parsed.get("result").is_some() {
                    parsed.get("result").cloned()
                } else {
                    None
                };
                Ok(PythonResult {
                    stdout: captured_stdout,
                    stderr,
                    result,
                    error,
                    timed_out: false,
                })
            }
            Ok(Err(error)) => Ok(PythonResult {
                stdout: String::new(),
                stderr: String::new(),
                result: None,
                error: Some(format!("IO error: {error}")),
                timed_out: false,
            }),
            Err(_) => Ok(PythonResult {
                stdout: String::new(),
                stderr: String::new(),
                result: None,
                error: Some(format!("timed out after {}s", self.timeout_secs)),
                timed_out: true,
            }),
        }
    }

    fn wrapper_code(user_code: &str) -> Result<String> {
        let code_literal = serde_json::to_string(user_code)?;
        Ok(format!(
            r#"
import contextlib, io, json, traceback
_code = {code_literal}
_stdout = io.StringIO()
_ns = {{}}
try:
    with contextlib.redirect_stdout(_stdout):
        exec(_code, _ns)
    print(json.dumps({{"stdout": _stdout.getvalue(), "result": _ns.get("result", None)}}, default=str))
except Exception as e:
    print(json.dumps({{"stdout": _stdout.getvalue(), "error": str(e), "traceback": traceback.format_exc()}}, default=str))
"#
        ))
    }
}

impl Default for PythonSandbox {
    fn default() -> Self {
        Self::new(60)
    }
}

fn micromamba_command() -> OsString {
    let explicit = PathBuf::from("/home/gilberthan/.local/bin/micromamba");
    if explicit.exists() {
        explicit.into_os_string()
    } else {
        std::ffi::OsString::from("micromamba")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_home_local_bin_micromamba_when_resolving_sandbox_binary() {
        let command = micromamba_command();
        assert_eq!(command.to_string_lossy(), "/home/gilberthan/.local/bin/micromamba");
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CodeExecuteInput {
    pub code: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

pub struct CodeExecuteTool {
    runtime: Arc<WorkspaceRuntime>,
}

impl CodeExecuteTool {
    pub fn new(runtime: Arc<WorkspaceRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl Tool for CodeExecuteTool {
    fn name(&self) -> &str {
        "code_execute"
    }

    fn description(&self) -> &str {
        "Execute Python 3.11 analysis code in the py311 micromamba environment. Set a JSON-serializable `result` variable to create canvas nodes. Supported result item kinds: markdown, data_table, plotly, heatmap."
    }

    fn schema(&self) -> RootSchema {
        schema_for!(CodeExecuteInput)
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> std::result::Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let input: CodeExecuteInput = match serde_json::from_value(args) {
            Ok(input) => input,
            Err(error) => {
                return Ok(ToolResult::new(
                    self.name(),
                    json!({"ok": false, "error": {"code": "invalid_input", "message": error.to_string()}}).to_string(),
                ));
            }
        };

        let sandbox = PythonSandbox::new(input.timeout_secs.unwrap_or(60));
        let result = match sandbox.execute(&input.code).await {
            Ok(result) => result,
            Err(error) => {
                return Ok(ToolResult::new(
                    self.name(),
                    json!({"ok": false, "error": {"code": "sandbox_error", "message": error.to_string()}}).to_string(),
                ));
            }
        };

        if result.timed_out || result.error.is_some() {
            return Ok(ToolResult::new(
                self.name(),
                json!({
                    "ok": false,
                    "error": {"code": if result.timed_out { "timeout" } else { "python_error" }, "message": result.error.clone().unwrap_or_default()},
                    "stdout": result.stdout,
                    "stderr": result.stderr,
                    "timed_out": result.timed_out,
                })
                .to_string(),
            ));
        }

        let items = result_items(result.result);
        let mut created = Vec::new();
        for (idx, item) in items.iter().enumerate() {
            let artifact_id = artifact_id_for(ctx, idx);
            let label = item_label(item, idx);
            let canvas_kind = canvas_kind_for(item);
            let format = format_for(item);
            let preview = preview_for(item);

            let mut produced_payload = Map::new();
            produced_payload.insert("artifact_id".to_string(), json!(artifact_id));
            produced_payload.insert("summary".to_string(), json!(label));
            produced_payload.insert("format".to_string(), json!(format));
            produced_payload.insert("canvas_kind".to_string(), json!(canvas_kind));
            produced_payload.insert("preview".to_string(), preview);
            produced_payload.insert("source".to_string(), json!("code_execute"));

            let produced = WorkflowEvent {
                id: uuid::Uuid::new_v4().to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                session_id: ctx.session_id.clone(),
                actor: "agent".to_string(),
                kind: WorkflowEventKind::ArtifactProduced,
                payload: Value::Object(produced_payload),
            };
            append_event_and_project(&self.runtime, produced).await?;

            let surfaced = WorkflowEvent {
                id: uuid::Uuid::new_v4().to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                session_id: ctx.session_id.clone(),
                actor: "agent".to_string(),
                kind: WorkflowEventKind::ArtifactSurfaced,
                payload: json!({
                    "artifact_id": artifact_id,
                    "canvas_node_id": format!("node_{artifact_id}"),
                    "position": {"x": 80.0 + (idx as f64 * 260.0), "y": 80.0},
                }),
            };
            append_event_and_project(&self.runtime, surfaced).await?;
            created.push(json!({"artifact_id": artifact_id, "canvas_node_id": format!("node_{artifact_id}"), "kind": canvas_kind}));
        }

        Ok(ToolResult::new(
            self.name(),
            json!({
                "ok": true,
                "nodes_created": created.len(),
                "artifacts": created,
                "stdout": result.stdout,
                "stderr": result.stderr,
            })
            .to_string(),
        ))
    }
}

fn result_items(result: Option<Value>) -> Vec<Value> {
    match result {
        Some(Value::Array(items)) => items,
        Some(Value::Null) | None => Vec::new(),
        Some(item) => vec![item],
    }
}

fn artifact_id_for(ctx: &ToolContext, idx: usize) -> String {
    let root = ctx
        .tool_call_id
        .as_deref()
        .unwrap_or(&ctx.message_id)
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    format!("code_execute_{root}_{idx}")
}

fn item_kind(item: &Value) -> &str {
    item.get("kind")
        .or_else(|| item.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("data_table")
}

fn canvas_kind_for(item: &Value) -> &'static str {
    match item_kind(item) {
        "markdown" | "text" => "markdown",
        "plotly" | "plotly_json" => "plotly",
        "heatmap" => "heatmap",
        "data_table" | "table" => "data_table",
        _ => "data_table",
    }
}

fn format_for(item: &Value) -> &'static str {
    match item_kind(item) {
        "markdown" | "text" => "markdown",
        "plotly" | "plotly_json" => "plotly",
        "heatmap" => "heatmap",
        "data_table" | "table" => "json",
        _ => "json",
    }
}

fn item_label(item: &Value, idx: usize) -> String {
    item.get("title")
        .or_else(|| item.get("label"))
        .or_else(|| item.get("summary"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("Code result {}", idx + 1))
}

fn preview_for(item: &Value) -> Value {
    match canvas_kind_for(item) {
        "markdown" => json!({"kind": "text", "content": markdown_content(item)}),
        "plotly" | "heatmap" => json!({"kind": "plot", "spec": plot_spec(item)}),
        _ => {
            let columns = table_columns(item);
            let column_keys = table_column_keys(item);
            let rows = table_rows(item, &column_keys);
            json!({"kind": "table", "columns": columns, "rows": rows, "total_rows": table_total_rows(item)})
        }
    }
}

fn markdown_content(item: &Value) -> String {
    item.get("content")
        .or_else(|| item.get("text"))
        .or_else(|| item.get("markdown"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn plot_spec(item: &Value) -> Value {
    if let Some(spec) = item.get("spec") {
        return spec.clone();
    }
    let mut spec = Map::new();
    if let Some(data) = item.get("data") {
        spec.insert("data".to_string(), data.clone());
    }
    if let Some(layout) = item.get("layout") {
        spec.insert("layout".to_string(), layout.clone());
    }
    if spec.is_empty() {
        item.clone()
    } else {
        Value::Object(spec)
    }
}

fn table_columns(item: &Value) -> Value {
    let Some(columns) = item.get("columns").and_then(Value::as_array) else {
        return json!([]);
    };
    let normalized: Vec<String> = columns
        .iter()
        .map(|column| {
            if let Some(header) = column.get("header").and_then(Value::as_str) {
                header.to_string()
            } else if let Some(field) = column.get("field").and_then(Value::as_str) {
                field.to_string()
            } else if let Some(name) = column.as_str() {
                name.to_string()
            } else {
                column.to_string()
            }
        })
        .collect();
    json!(normalized)
}

fn table_column_keys(item: &Value) -> Value {
    let Some(columns) = item.get("columns").and_then(Value::as_array) else {
        return json!([]);
    };
    let normalized: Vec<String> = columns
        .iter()
        .map(|column| {
            if let Some(field) = column.get("field").and_then(Value::as_str) {
                field.to_string()
            } else if let Some(header) = column.get("header").and_then(Value::as_str) {
                header.to_string()
            } else if let Some(name) = column.as_str() {
                name.to_string()
            } else {
                column.to_string()
            }
        })
        .collect();
    json!(normalized)
}

fn table_rows(item: &Value, column_keys: &Value) -> Value {
    let Some(row_values) = item.get("rows").and_then(Value::as_array) else {
        return json!([]);
    };
    let col_names: Vec<String> = column_keys
        .as_array()
        .map(|cols| {
            cols.iter()
                .filter_map(|column| column.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let normalized: Vec<Vec<Value>> = row_values
        .iter()
        .map(|row| {
            if let Some(obj) = row.as_object() {
                if col_names.is_empty() {
                    return obj.values().cloned().collect();
                }
                col_names
                    .iter()
                    .map(|name| obj.get(name).cloned().unwrap_or(Value::Null))
                    .collect()
            } else if let Some(arr) = row.as_array() {
                arr.clone()
            } else {
                vec![row.clone()]
            }
        })
        .collect();
    json!(normalized)
}

fn table_total_rows(item: &Value) -> usize {
    item.get("total_rows")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .or_else(|| item.get("rows").and_then(Value::as_array).map(Vec::len))
        .unwrap_or(0)
}
