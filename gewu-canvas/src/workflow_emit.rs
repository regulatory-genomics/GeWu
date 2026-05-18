use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowEmitInput {
    /// Event kind: data_ingested, step_executed, artifact_produced, artifact_surfaced, edge_declared, etc.
    pub kind: String,
    /// Event payload. Shape depends on kind.
    pub payload: Value,
    /// Session ID. Defaults to "default".
    #[serde(default = "default_session")]
    pub session_id: String,
}

fn default_session() -> String {
    "default".to_string()
}

fn normalize_payload_aliases(kind: &str, payload: Value) -> Value {
    let Value::Object(mut map) = payload else {
        return payload;
    };

    if let Some(id) = map.get("id").cloned() {
        let canonical = match kind {
            "data_ingested" => Some("data_id"),
            "step_executed" => Some("step_id"),
            "artifact_produced" | "artifact_surfaced" | "artifact_hidden" => Some("artifact_id"),
            "annotation_added" => Some("target_id"),
            _ => None,
        };
        if let Some(key) = canonical {
            map.entry(key.to_string()).or_insert(id);
        }
    }

    if kind == "edge_declared" {
        if let Some(from) = map.get("from").cloned() {
            map.entry("from_id".to_string()).or_insert(from);
        }
        if let Some(to) = map.get("to").cloned() {
            map.entry("to_id".to_string()).or_insert(to);
        }
    }

    Value::Object(map)
}

pub struct WorkflowHttpEmitTool {
    base_url: String,
    client: reqwest::Client,
}

pub type WorkflowEmitTool = WorkflowHttpEmitTool;

impl WorkflowHttpEmitTool {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Tool for WorkflowHttpEmitTool {
    fn name(&self) -> &str {
        "workflow.emit"
    }

    fn description(&self) -> &str {
        "Emit a workflow event to tachyon-canvas. Use this to record data ingestion, analysis steps, artifacts, canvas surfacing, and lineage edges."
    }

    fn schema(&self) -> RootSchema {
        schema_for!(WorkflowEmitInput)
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let input: WorkflowEmitInput = match serde_json::from_value(args) {
            Ok(input) => input,
            Err(error) => {
                return Ok(ToolResult::new(
                    self.name(),
                    json!({ "ok": false, "error": { "code": "invalid_input", "message": error.to_string() } }).to_string(),
                ));
            }
        };

        let body = json!({
            "kind": input.kind,
            "actor": "agent",
            "session_id": input.session_id,
            "payload": normalize_payload_aliases(&input.kind, input.payload),
        });
        let url = format!("{}/api/workflow/events", self.base_url);

        let response = match self.client.post(&url).json(&body).send().await {
            Ok(response) => response,
            Err(error) => {
                return Ok(ToolResult::new(
                    self.name(),
                    json!({ "ok": false, "error": { "code": "request_failed", "message": error.to_string() } }).to_string(),
                ));
            }
        };

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if status.is_success() {
            Ok(ToolResult::new(
                self.name(),
                json!({ "ok": true, "data": { "status": status.as_u16(), "response": text } })
                    .to_string(),
            ))
        } else {
            Ok(ToolResult::new(
                self.name(),
                json!({ "ok": false, "error": { "code": "http_error", "message": text, "status": status.as_u16() } }).to_string(),
            ))
        }
    }
}
