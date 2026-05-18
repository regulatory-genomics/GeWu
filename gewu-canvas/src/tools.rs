//! Canvas `Tool` implementations exposing canvas and tachyon workflow operations to the agent.
//!
//! All tools share an `Arc<CanvasManager>`. Session scoping comes from
//! `ToolContext::session_id` at call time, so a single set of tool instances
//! can serve every session.
//!
//! ## SSE wiring
//!
//! Per `tachyon-canvas/docs/sse-bridge.md`:
//!
//!   - `canvas.open` returns a `CanvasEvent::Open` JSON object from
//!     `start_metadata`. GeWu's chat handler forwards `tool_start.metadata`
//!     to the browser, where the canvas controller mounts the overlay.
//!   - `canvas.set_state` / `canvas.close` / `canvas.focus` write their
//!     `CanvasEvent::*` JSON into `ToolResult::output`. The frontend already
//!     parses this string with `parseToolOutputJson`.
//!   - `canvas.get_state` / `canvas.list` are pure agent reads; they do not
//!     emit canvas events.

use std::error::Error;
use std::sync::Arc;

use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolExecutionPolicy, ToolResult};
use schemars::schema::RootSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manager::{CanvasError, CanvasManager};
use crate::state::{
    CanvasEvent, CanvasKind, CanvasMode, CanvasState, CanvasStatePatch, GenomeCanvasOpenRequest,
    build_genome_canvas_payload,
};

use crate::workflow_emit::WorkflowEmitTool;

/// Build the canonical canvas tools, sharing a single manager.
pub fn canvas_tools(manager: Arc<CanvasManager>) -> Vec<Arc<dyn Tool>> {
    let workflow_url = std::env::var("GEWU_TACHYON_CANVAS_URL")
        .unwrap_or_else(|_| "http://localhost:3210".to_string());
    vec![
        Arc::new(CanvasOpenTool {
            manager: manager.clone(),
        }),
        Arc::new(CanvasOpenGenomeTool {
            manager: manager.clone(),
        }),
        Arc::new(CanvasCloseTool {
            manager: manager.clone(),
        }),
        Arc::new(CanvasFocusTool {
            manager: manager.clone(),
        }),
        Arc::new(CanvasSetStateTool {
            manager: manager.clone(),
        }),
        Arc::new(CanvasGetStateTool {
            manager: manager.clone(),
        }),
        Arc::new(CanvasListTool { manager }),
        Arc::new(WorkflowEmitTool::new(workflow_url)),
    ]
}

fn schema_from(json_value: Value) -> RootSchema {
    serde_json::from_value(json_value).expect("static schema must parse")
}

fn box_err<E: Into<Box<dyn Error + Send + Sync>>>(e: E) -> Box<dyn Error + Send + Sync> {
    e.into()
}

// ---------------------------------------------------------------------------
// canvas.open
// ---------------------------------------------------------------------------

pub struct CanvasOpenTool {
    pub manager: Arc<CanvasManager>,
}

#[derive(Deserialize)]
struct OpenArgs {
    kind: CanvasKind,
    #[serde(default)]
    initial_state: Option<Value>,
    #[serde(default)]
    mode: Option<CanvasMode>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    canvas_id: Option<String>,
}

#[async_trait]
impl Tool for CanvasOpenTool {
    fn name(&self) -> &str {
        "canvas.open"
    }
    fn description(&self) -> &str {
        "Open a new canvas surface (or focus an existing one if canvas_id is provided). \
         By default the canvas appears docked in the user's Work Panel Canvas tab; use \
         mode=\"overlay\" for an optional popup. Use kind=\"genome\" for a genomic browser \
         canvas. The canvas is per-session; use canvas.list to inspect what is open and \
         canvas.get_state to read what the user is currently viewing."
    }
    fn schema(&self) -> RootSchema {
        schema_from(json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["genome", "hic", "umap", "table", "report", "custom"],
                    "description": "Canvas kind. MVP supports only \"genome\"."
                },
                "initial_state": {
                    "type": "object",
                    "description": "Kind-specific payload. For genome: {region, samples, tracks, ...}."
                },
                "mode": {
                    "type": "string",
                    "enum": ["overlay", "docked", "fullscreen"],
                "description": "Display mode (default docked). Use overlay for an optional popup."
                },
                "title": { "type": "string" },
                "canvas_id": {
                    "type": "string",
                    "description": "Pass to reuse an existing canvas instead of creating a new one."
                }
            },
            "required": ["kind"]
        }))
    }
    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::ParallelSafe
    }

    /// Pre-flight metadata: the frontend uses this in `tool_start.metadata`
    /// to mount the overlay BEFORE `execute` runs. The data here is a best-
    /// effort preview; `execute` returns the authoritative state.
    fn start_metadata(
        &self,
        args: &str,
        _ctx: &ToolContext,
    ) -> Result<Value, Box<dyn Error + Send + Sync>> {
        let parsed: OpenArgs = serde_json::from_str(args).map_err(box_err)?;
        let preview = preview_canvas_state(&parsed)?;
        let event = CanvasEvent::Open {
            canvas_id: preview.canvas_id.clone(),
            kind: preview.kind,
            mode: preview.mode,
            initial_state: preview,
        };
        serde_json::to_value(event).map_err(box_err)
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: OpenArgs = serde_json::from_value(args).map_err(box_err)?;
        let mode = input.mode.unwrap_or_default();
        let state = match self
            .manager
            .open(
                &ctx.session_id,
                input.canvas_id.clone(),
                input.kind,
                mode,
                input.title,
                input.initial_state,
            )
            .await
        {
            Ok(state) => state,
            Err(e) => return Ok(ToolResult::new("canvas.open", error_output(e))),
        };

        // Tool result string carries the same canvas:open event the frontend
        // already mounted from start_metadata, plus the authoritative state
        // (with the real canvas_id, in case start_metadata invented one).
        let event = CanvasEvent::Open {
            canvas_id: state.canvas_id.clone(),
            kind: state.kind,
            mode: state.mode,
            initial_state: state.clone(),
        };
        let output = json!({
            "canvas_id": state.canvas_id,
            "state": state,
            // Duplicate the event in the output so the frontend's
            // tool_finish handler can reconcile if start_metadata was lost.
            "event": "canvas:open",
            "event_payload": event,
        });
        Ok(ToolResult::new("canvas.open", output.to_string()))
    }
}

fn preview_canvas_state(args: &OpenArgs) -> Result<CanvasState, CanvasError> {
    let id = args
        .canvas_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or("pending")
        .to_string();
    let mut state = CanvasState::new(id, args.kind);
    state.mode = args.mode.unwrap_or_default();
    state.title = args.title.clone().filter(|t| !t.is_empty());
    if let Some(payload) = &args.initial_state {
        state.payload = payload.clone();
        state.validate_payload()?;
    }
    Ok(state)
}

// ---------------------------------------------------------------------------
// canvas.open_genome
// ---------------------------------------------------------------------------

pub struct CanvasOpenGenomeTool {
    pub manager: Arc<CanvasManager>,
}

#[derive(Deserialize)]
struct OpenGenomeArgs {
    #[serde(flatten)]
    request: GenomeCanvasOpenRequest,
    #[serde(default)]
    mode: Option<CanvasMode>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    canvas_id: Option<String>,
}

#[async_trait]
impl Tool for CanvasOpenGenomeTool {
    fn name(&self) -> &str {
        "canvas.open_genome"
    }
    fn description(&self) -> &str {
        "Open a genome canvas with a canonical payload. Prefer this over hand-writing \
         canvas.open payloads for genome views. Samples use `id`; tracks use `sample_id`."
    }
    fn schema(&self) -> RootSchema {
        schema_from(json!({
            "type": "object",
            "properties": {
                "assembly": {
                    "type": "string",
                    "description": "Genome assembly (e.g., hg38, mm10)."
                },
                "region": {
                    "type": "string",
                    "description": "Genomic region (e.g., chr1:1-100)."
                },
                "gene_symbol": { "type": "string" },
                "title": { "type": "string" },
                "mode": {
                    "type": "string",
                    "enum": ["overlay", "docked", "fullscreen"]
                },
                "canvas_id": { "type": "string" },
                "samples": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "label": { "type": "string" },
                            "assay": { "type": "string" },
                            "uri": { "type": "string", "pattern": "^gewu://[^/]+/[^/]+$" }
                        },
                        "required": ["id"],
                        "additionalProperties": false
                    }
                },
                "tracks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "type": {
                                "type": "string",
                                "enum": ["signal", "peak", "gene", "annotation", "qc"]
                            },
                            "source": { "type": "string" },
                            "sample_id": { "type": "string" },
                            "assay": { "type": "string" },
                            "uri": { "type": "string", "pattern": "^gewu://[^/]+/[^/]+$" },
                            "path": { "type": "string" },
                            "url": { "type": "string" }
                        },
                        "required": ["id", "type", "sample_id"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["assembly", "region"],
            "additionalProperties": false
        }))
    }
    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::ParallelSafe
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: OpenGenomeArgs = serde_json::from_value(args).map_err(box_err)?;

        let payload = match build_genome_canvas_payload(input.request) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::new("canvas.open_genome", error_output(e))),
        };

        let mode = input.mode.unwrap_or_default();
        let state = match self
            .manager
            .open(
                &ctx.session_id,
                input.canvas_id.clone(),
                CanvasKind::Genome,
                mode,
                input.title,
                Some(payload),
            )
            .await
        {
            Ok(state) => state,
            Err(e) => return Ok(ToolResult::new("canvas.open_genome", error_output(e))),
        };

        let event = CanvasEvent::Open {
            canvas_id: state.canvas_id.clone(),
            kind: state.kind,
            mode: state.mode,
            initial_state: state.clone(),
        };
        let output = json!({
            "canvas_id": state.canvas_id,
            "state": state,
            "event": "canvas:open",
            "event_payload": event,
            "next_actions": [{
                "tool": "canvas.list",
                "args": {},
                "purpose": "Verify the canvas is visible in this session."
            }]
        });
        Ok(ToolResult::new("canvas.open_genome", output.to_string()))
    }
}

// ---------------------------------------------------------------------------
// canvas.close
// ---------------------------------------------------------------------------

pub struct CanvasCloseTool {
    pub manager: Arc<CanvasManager>,
}

#[derive(Deserialize)]
struct CanvasIdArgs {
    canvas_id: String,
}

#[async_trait]
impl Tool for CanvasCloseTool {
    fn name(&self) -> &str {
        "canvas.close"
    }
    fn description(&self) -> &str {
        "Close a canvas surface. The user can also close it manually; calling this again on a \
         missing canvas returns canvas_not_found."
    }
    fn schema(&self) -> RootSchema {
        schema_from(json!({
            "type": "object",
            "properties": { "canvas_id": { "type": "string" } },
            "required": ["canvas_id"]
        }))
    }
    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::ParallelSafe
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: CanvasIdArgs = serde_json::from_value(args).map_err(box_err)?;
        match self.manager.close(&ctx.session_id, &input.canvas_id).await {
            Ok(state) => Ok(ToolResult::new(
                "canvas.close",
                json!({
                    "canvas_id": input.canvas_id,
                    "closed": true,
                    "state": state,
                    "event": "canvas:close",
                    "event_payload": {
                        "event": "canvas:close",
                        "canvas_id": input.canvas_id,
                        "state": state,
                    },
                })
                .to_string(),
            )),
            Err(e) => Ok(ToolResult::new(
                "canvas.close",
                json!({
                    "error": { "code": "canvas_not_found", "message": e.to_string() }
                })
                .to_string(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// canvas.focus
// ---------------------------------------------------------------------------

pub struct CanvasFocusTool {
    pub manager: Arc<CanvasManager>,
}

#[async_trait]
impl Tool for CanvasFocusTool {
    fn name(&self) -> &str {
        "canvas.focus"
    }
    fn description(&self) -> &str {
        "Bring an existing canvas to the front and mark it as focused."
    }
    fn schema(&self) -> RootSchema {
        schema_from(json!({
            "type": "object",
            "properties": { "canvas_id": { "type": "string" } },
            "required": ["canvas_id"]
        }))
    }
    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::ParallelSafe
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: CanvasIdArgs = serde_json::from_value(args).map_err(box_err)?;
        match self.manager.focus(&ctx.session_id, &input.canvas_id).await {
            Ok(state) => Ok(ToolResult::new(
                "canvas.focus",
                json!({
                    "canvas_id": state.canvas_id,
                    "state": state,
                    "event": "canvas:focus",
                })
                .to_string(),
            )),
            Err(e) => Ok(ToolResult::new(
                "canvas.focus",
                json!({
                    "error": { "code": "canvas_not_found", "message": e.to_string() }
                })
                .to_string(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// canvas.set_state
// ---------------------------------------------------------------------------

pub struct CanvasSetStateTool {
    pub manager: Arc<CanvasManager>,
}

#[derive(Deserialize)]
struct SetStateArgs {
    canvas_id: String,
    patch: CanvasStatePatch,
}

#[async_trait]
impl Tool for CanvasSetStateTool {
    fn name(&self) -> &str {
        "canvas.set_state"
    }
    fn description(&self) -> &str {
        "Apply a patch to an existing canvas. Top-level fields (title, mode, visible, focused) \
         are replaced. The `payload` object is shallow-merged: each key in patch.payload \
         replaces the corresponding key in the live state. To clear a payload key, set it to null. \
         Use this to navigate the genome canvas (patch.payload.region), update samples/tracks, \
         or attach an evidence bundle (patch.payload.evidence)."
    }
    fn schema(&self) -> RootSchema {
        schema_from(json!({
            "type": "object",
            "properties": {
                "canvas_id": { "type": "string" },
                "patch": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" },
                        "mode": { "type": "string", "enum": ["overlay", "docked", "fullscreen"] },
                        "visible": { "type": "boolean" },
                        "focused": { "type": "boolean" },
                        "payload": { "type": "object", "additionalProperties": true }
                    },
                    "additionalProperties": false
                }
            },
            "required": ["canvas_id", "patch"]
        }))
    }
    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::Serial
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: SetStateArgs = serde_json::from_value(args).map_err(box_err)?;
        match self
            .manager
            .set_state(&ctx.session_id, &input.canvas_id, input.patch.clone())
            .await
        {
            Ok(state) => {
                let event = CanvasEvent::Patch {
                    canvas_id: state.canvas_id.clone(),
                    patch: input.patch,
                    updated_at: state.updated_at.clone(),
                };
                let output = json!({
                    "canvas_id": state.canvas_id,
                    "state": state,
                    "event": "canvas:patch",
                    "event_payload": event,
                });
                Ok(ToolResult::new("canvas.set_state", output.to_string()))
            }
            Err(e) => Ok(ToolResult::new("canvas.set_state", error_output(e))),
        }
    }
}

fn error_output(error: CanvasError) -> String {
    match error {
        CanvasError::NotFound { .. } => json!({
            "error": { "code": "canvas_not_found", "message": error.to_string() }
        })
        .to_string(),
        CanvasError::InvalidPatch(_) => json!({
            "error": { "code": "invalid_patch", "message": error.to_string() }
        })
        .to_string(),
        CanvasError::InvalidGenomeCanvasPayload(details) => json!({
            "error": {
                "code": if details.iter().any(|issue| issue.code == "source_invalid") {
                    "source_invalid"
                } else {
                    "invalid_genome_canvas_payload"
                },
                "message": details
                    .iter()
                    .find(|issue| issue.code == "source_invalid")
                    .map(|issue| issue.message.clone())
                    .unwrap_or_else(|| "invalid genome canvas payload".to_string()),
                "details": details
            }
        })
        .to_string(),
    }
}

// ---------------------------------------------------------------------------
// canvas.get_state
// ---------------------------------------------------------------------------

pub struct CanvasGetStateTool {
    pub manager: Arc<CanvasManager>,
}

#[async_trait]
impl Tool for CanvasGetStateTool {
    fn name(&self) -> &str {
        "canvas.get_state"
    }
    fn description(&self) -> &str {
        "Read the current state of a canvas. Use BEFORE running domain tools (e.g. \
         genome.inspect_region) to find out what region/samples/tracks the user is currently \
         viewing. Returns null state when the canvas has been closed."
    }
    fn schema(&self) -> RootSchema {
        schema_from(json!({
            "type": "object",
            "properties": { "canvas_id": { "type": "string" } },
            "required": ["canvas_id"]
        }))
    }
    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::ParallelSafe
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: CanvasIdArgs = serde_json::from_value(args).map_err(box_err)?;
        let state = self
            .manager
            .get_state(&ctx.session_id, &input.canvas_id)
            .await;
        let output = json!({
            "canvas_id": input.canvas_id,
            "state": state,
        });
        Ok(ToolResult::new("canvas.get_state", output.to_string()))
    }
}

// ---------------------------------------------------------------------------
// canvas.list
// ---------------------------------------------------------------------------

pub struct CanvasListTool {
    pub manager: Arc<CanvasManager>,
}

#[async_trait]
impl Tool for CanvasListTool {
    fn name(&self) -> &str {
        "canvas.list"
    }
    fn description(&self) -> &str {
        "List all canvases currently open in this session."
    }
    fn schema(&self) -> RootSchema {
        schema_from(json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }))
    }
    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::ParallelSafe
    }

    async fn execute(
        &self,
        _args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let canvases = self.manager.list(&ctx.session_id).await;
        Ok(ToolResult::new(
            "canvas.list",
            json!({ "canvases": canvases }).to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(session: &str) -> ToolContext {
        ToolContext::new(session, "msg-1")
    }

    fn error_details(output: &str) -> Value {
        serde_json::from_str::<Value>(output).unwrap()["error"]["details"].clone()
    }

    fn assert_validation_issue(details: &Value, path: &str, code: &str) {
        let issues = details.as_array().unwrap();
        assert!(
            issues
                .iter()
                .any(|issue| issue["path"] == path && issue["code"] == code),
            "expected issue {path}/{code}, got {details}"
        );
    }

    #[tokio::test]
    async fn open_genome_blank_canvas_id_allocates_visible_canvas_id() {
        let m = Arc::new(CanvasManager::new());
        let open_genome = CanvasOpenGenomeTool { manager: m.clone() };

        let result = open_genome
            .execute(
                json!({
                    "assembly": "hg38",
                    "region": "chr16:8885000-9079500",
                    "gene_symbol": "SP7",
                    "canvas_id": "",
                    "samples": [
                        { "id": "SP4-1_ATAC13", "label": "SP4-1_ATAC13", "assay": "atac" }
                    ],
                    "tracks": [
                        {
                            "id": "signal-atac-SP4-1_ATAC13",
                            "type": "signal",
                            "sample_id": "SP4-1_ATAC13",
                            "assay": "atac",
                            "source": "l1"
                        }
                    ]
                }),
                &ctx("s1"),
            )
            .await
            .unwrap();

        let opened: Value = serde_json::from_str(&result.output).unwrap();
        let canvas_id = opened["canvas_id"].as_str().unwrap();
        assert!(
            !canvas_id.trim().is_empty(),
            "canvas_id must be frontend-addressable"
        );
        assert_eq!(opened["event_payload"]["canvas_id"], canvas_id);
        assert_eq!(opened["state"]["canvas_id"], canvas_id);
        assert_eq!(opened["state"]["payload"]["tracks"][0]["source"], "l1");
        assert!(m.get_state("s1", canvas_id).await.is_some());
    }

    #[tokio::test]
    async fn open_then_get_state_round_trip() {
        let m = Arc::new(CanvasManager::new());
        let tools = canvas_tools(m.clone());
        let open = tools.iter().find(|t| t.name() == "canvas.open").unwrap();
        let get = tools
            .iter()
            .find(|t| t.name() == "canvas.get_state")
            .unwrap();

        let opened_raw = open
            .execute(
                json!({ "kind": "genome", "initial_state": { "assembly": "hg38", "region": "chr1:1-100" } }),
                &ctx("s1"),
            )
            .await
            .unwrap();
        let opened: Value = serde_json::from_str(&opened_raw.output).unwrap();
        let canvas_id = opened["canvas_id"].as_str().unwrap();

        let got = get
            .execute(json!({ "canvas_id": canvas_id }), &ctx("s1"))
            .await
            .unwrap();
        let got_json: Value = serde_json::from_str(&got.output).unwrap();
        assert_eq!(got_json["state"]["payload"]["region"], "chr1:1-100");
    }

    #[tokio::test]
    async fn open_rejects_malformed_genome_payload_with_structured_error() {
        let m = Arc::new(CanvasManager::new());
        let open = CanvasOpenTool { manager: m.clone() };

        let result = open
            .execute(
                json!({
                    "kind": "genome",
                    "initial_state": {
                        "assembly": "hg38",
                        "region": "chr1:1-100",
                        "samples": [{ "sample_id": "tumor" }],
                        "tracks": [{ "id": "t1", "type": "signal", "sample_id": "tumor" }]
                    }
                }),
                &ctx("s1"),
            )
            .await
            .unwrap();

        let json: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(json["error"]["code"], "invalid_genome_canvas_payload");
        assert_validation_issue(
            &json["error"]["details"],
            "samples[0].id",
            "missing_required",
        );
        assert_validation_issue(
            &json["error"]["details"],
            "samples[0].sample_id",
            "unexpected_field",
        );
        assert!(m.list("s1").await.is_empty());
    }

    #[tokio::test]
    async fn open_genome_canvas_accepts_resolved_spi1_payload() {
        let m = Arc::new(CanvasManager::new());
        let tools = canvas_tools(m.clone());
        let open = tools.iter().find(|t| t.name() == "canvas.open").unwrap();
        let get = tools
            .iter()
            .find(|t| t.name() == "canvas.get_state")
            .unwrap();

        let payload = json!({
            "assembly": "hg38",
            "region": "chr11:47354860-47395607",
            "samples": [
                { "id": "AS1-1_ATAC10", "label": "AS1-1_ATAC10", "assay": "atac" }
            ],
            "tracks": [
                {
                    "id": "signal-atac-AS1-1_ATAC10",
                    "type": "signal",
                    "sample_id": "AS1-1_ATAC10",
                    "assay": "atac",
                    "source": "coverage",
                    "visible": true
                }
            ]
        });
        let opened_raw = open
            .execute(
                json!({ "kind": "genome", "title": "SPI1 ATAC", "initial_state": payload }),
                &ctx("s1"),
            )
            .await
            .unwrap();
        let opened: Value = serde_json::from_str(&opened_raw.output).unwrap();
        let canvas_id = opened["canvas_id"].as_str().unwrap();

        assert_eq!(opened["event"], "canvas:open");
        assert_eq!(
            opened["state"]["payload"]["region"],
            "chr11:47354860-47395607"
        );
        assert_eq!(
            opened["state"]["payload"]["tracks"][0]["source"],
            "coverage"
        );

        let got = get
            .execute(json!({ "canvas_id": canvas_id }), &ctx("s1"))
            .await
            .unwrap();
        let got_json: Value = serde_json::from_str(&got.output).unwrap();
        assert_eq!(got_json["state"]["payload"]["assembly"], "hg38");
        assert_eq!(
            got_json["state"]["payload"]["samples"][0]["id"],
            "AS1-1_ATAC10"
        );
    }

    #[tokio::test]
    async fn open_genome_rejects_ucf_source_with_recovery_hint() {
        let open_genome = CanvasOpenGenomeTool {
            manager: Arc::new(CanvasManager::new()),
        };

        let result = open_genome
            .execute(
                json!({
                    "assembly": "hg38",
                    "region": "chr11:47354860-47395607",
                    "samples": [
                        { "id": "E3-1_ATAC12", "label": "E3-1_ATAC12", "assay": "atac" }
                    ],
                    "tracks": [
                        {
                            "id": "signal-atac-E3-1_ATAC12",
                            "type": "signal",
                            "sample_id": "E3-1_ATAC12",
                            "assay": "atac",
                            "source": "ucf"
                        }
                    ]
                }),
                &ctx("s1"),
            )
            .await
            .unwrap();

        let json: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(json["error"]["code"], "source_invalid");
        assert!(
            json["error"]["details"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["code"] == "source_invalid")
        );
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("coverage")
        );
    }

    #[tokio::test]
    async fn open_rejects_missing_assembly() {
        let open = CanvasOpenTool {
            manager: Arc::new(CanvasManager::new()),
        };

        let result = open
            .execute(
                json!({
                    "kind": "genome",
                    "initial_state": { "region": "chr1:1-100" }
                }),
                &ctx("s1"),
            )
            .await
            .unwrap();

        assert_validation_issue(
            &error_details(&result.output),
            "assembly",
            "missing_required",
        );
    }

    #[tokio::test]
    async fn open_rejects_empty_region() {
        let open = CanvasOpenTool {
            manager: Arc::new(CanvasManager::new()),
        };

        let result = open
            .execute(
                json!({
                    "kind": "genome",
                    "initial_state": { "assembly": "hg38", "region": "" }
                }),
                &ctx("s1"),
            )
            .await
            .unwrap();

        assert_validation_issue(&error_details(&result.output), "region", "missing_required");
    }

    #[tokio::test]
    async fn open_rejects_empty_assembly() {
        let open = CanvasOpenTool {
            manager: Arc::new(CanvasManager::new()),
        };

        let result = open
            .execute(
                json!({
                    "kind": "genome",
                    "initial_state": { "assembly": "", "region": "chr1:1-100" }
                }),
                &ctx("s1"),
            )
            .await
            .unwrap();

        assert_validation_issue(
            &error_details(&result.output),
            "assembly",
            "missing_required",
        );
    }

    #[tokio::test]
    async fn reopening_existing_with_invalid_initial_state_preserves_previous_payload() {
        let m = Arc::new(CanvasManager::new());
        m.open(
            "s1",
            Some("c-1".into()),
            CanvasKind::Genome,
            CanvasMode::Overlay,
            None,
            Some(json!({
                "assembly": "hg38",
                "region": "chr1:1-100",
                "samples": [{ "id": "normal" }],
                "tracks": []
            })),
        )
        .await
        .unwrap();
        let open = CanvasOpenTool { manager: m.clone() };

        let result = open
            .execute(
                json!({
                    "kind": "genome",
                    "canvas_id": "c-1",
                    "initial_state": {
                        "assembly": "hg38",
                        "region": "chr2:1-100",
                        "tracks": [{ "id": "t1", "type": "signal", "sample_id": "missing" }]
                    }
                }),
                &ctx("s1"),
            )
            .await
            .unwrap();

        let json: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(json["error"]["code"], "invalid_genome_canvas_payload");
        assert_validation_issue(
            &json["error"]["details"],
            "tracks[0].sample_id",
            "unknown_reference",
        );

        let stored = m.get_state("s1", "c-1").await.unwrap();
        assert_eq!(stored.payload["region"], "chr1:1-100");
        assert_eq!(stored.payload["samples"][0]["id"], "normal");
        assert_eq!(stored.payload["tracks"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn reopening_existing_with_invalid_initial_state_preserves_canvas_list_state() {
        let m = Arc::new(CanvasManager::new());
        m.open(
            "s1",
            Some("target".into()),
            CanvasKind::Genome,
            CanvasMode::Overlay,
            Some("original title".into()),
            Some(json!({
                "assembly": "hg38",
                "region": "chr1:1-100",
                "samples": [{ "id": "normal" }],
                "tracks": []
            })),
        )
        .await
        .unwrap();
        m.open(
            "s1",
            Some("sibling".into()),
            CanvasKind::Genome,
            CanvasMode::Overlay,
            Some("sibling title".into()),
            None,
        )
        .await
        .unwrap();
        let open = CanvasOpenTool { manager: m.clone() };

        let result = open
            .execute(
                json!({
                    "kind": "genome",
                    "canvas_id": "target",
                    "title": "mutated title",
                    "initial_state": {
                        "assembly": "hg38",
                        "region": "chr2:1-100",
                        "tracks": [{ "id": "bad", "type": "signal", "sample_id": "missing" }]
                    }
                }),
                &ctx("s1"),
            )
            .await
            .unwrap();

        let json: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(json["error"]["code"], "invalid_genome_canvas_payload");
        assert_validation_issue(
            &json["error"]["details"],
            "tracks[0].sample_id",
            "unknown_reference",
        );

        let target = m.get_state("s1", "target").await.unwrap();
        assert_eq!(target.title.as_deref(), Some("original title"));
        assert!(!target.focused);
        assert!(target.visible);
        assert_eq!(target.payload["region"], "chr1:1-100");

        let sibling = m.get_state("s1", "sibling").await.unwrap();
        assert_eq!(sibling.title.as_deref(), Some("sibling title"));
        assert!(sibling.focused);
        assert!(sibling.visible);
    }

    #[test]
    fn start_metadata_reports_malformed_genome_payload_for_direct_callers() {
        let open = CanvasOpenTool {
            manager: Arc::new(CanvasManager::new()),
        };
        let args = serde_json::to_string(&json!({
            "kind": "genome",
            "initial_state": { "assembly": "hg38", "samples": [] }
        }))
        .unwrap();

        let error = open.start_metadata(&args, &ctx("s1")).unwrap_err();

        assert!(error.to_string().contains("invalid genome canvas payload"));
    }

    #[tokio::test]
    async fn set_state_rejects_mismatched_genome_track_sample_without_mutating_state() {
        let m = Arc::new(CanvasManager::new());
        let opened = m
            .open(
                "s1",
                Some("c-1".into()),
                CanvasKind::Genome,
                CanvasMode::Overlay,
                None,
                Some(json!({
                    "assembly": "hg38",
                    "region": "chr1:1-100",
                    "samples": [{ "id": "normal" }],
                    "tracks": []
                })),
            )
            .await
            .unwrap();
        let set = CanvasSetStateTool { manager: m.clone() };

        let result = set
            .execute(
                json!({
                    "canvas_id": opened.canvas_id,
                    "patch": { "payload": { "tracks": [{ "id": "t1", "type": "signal", "sample_id": "tumor" }] } }
                }),
                &ctx("s1"),
            )
            .await
            .unwrap();

        let json: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(json["error"]["code"], "invalid_genome_canvas_payload");
        assert_eq!(json["error"]["details"][0]["path"], "tracks[0].sample_id");
        assert_eq!(json["error"]["details"][0]["code"], "unknown_reference");

        let stored = m.get_state("s1", "c-1").await.unwrap();
        assert_eq!(stored.payload["tracks"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn start_metadata_emits_canvas_open_event() {
        let m = Arc::new(CanvasManager::new());
        let open = CanvasOpenTool { manager: m.clone() };
        let args = serde_json::to_string(&json!({
            "kind": "genome",
            "title": "demo",
            "canvas_id": "c-known"
        }))
        .unwrap();
        let meta = open.start_metadata(&args, &ctx("s1")).unwrap();
        assert_eq!(meta["event"], "canvas:open");
        assert_eq!(meta["canvas_id"], "c-known");
        assert_eq!(meta["kind"], "genome");
        assert_eq!(meta["initial_state"]["title"], "demo");
    }

    #[tokio::test]
    async fn set_state_emits_patch_event_in_output() {
        let m = Arc::new(CanvasManager::new());
        let opened = m
            .open(
                "s1",
                Some("c-1".into()),
                CanvasKind::Genome,
                CanvasMode::Overlay,
                None,
                Some(json!({"assembly": "hg38", "region": "chr1:1-100"})),
            )
            .await
            .unwrap();
        let set = CanvasSetStateTool { manager: m };
        let result = set
            .execute(
                json!({
                    "canvas_id": opened.canvas_id,
                    "patch": { "payload": { "region": "chr2:1-100" } }
                }),
                &ctx("s1"),
            )
            .await
            .unwrap();
        let json: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(json["event"], "canvas:patch");
        assert_eq!(json["event_payload"]["event"], "canvas:patch");
        assert_eq!(json["state"]["payload"]["region"], "chr2:1-100");
    }

    #[tokio::test]
    async fn list_returns_canvases_for_session_only() {
        let m = Arc::new(CanvasManager::new());
        m.open(
            "s1",
            None,
            CanvasKind::Genome,
            CanvasMode::Overlay,
            None,
            None,
        )
        .await
        .unwrap();
        m.open(
            "s2",
            None,
            CanvasKind::Genome,
            CanvasMode::Overlay,
            None,
            None,
        )
        .await
        .unwrap();
        let list = CanvasListTool { manager: m };

        let r1 = list.execute(json!({}), &ctx("s1")).await.unwrap();
        let j1: Value = serde_json::from_str(&r1.output).unwrap();
        assert_eq!(j1["canvases"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn get_state_missing_canvas_returns_null_not_error() {
        let m = Arc::new(CanvasManager::new());
        let get = CanvasGetStateTool { manager: m };
        let result = get
            .execute(json!({ "canvas_id": "nope" }), &ctx("s1"))
            .await
            .unwrap();
        let json: Value = serde_json::from_str(&result.output).unwrap();
        assert!(json["state"].is_null());
    }

    #[tokio::test]
    async fn open_genome_tool_builds_canonical_payload_and_emits_open_event() {
        let m = Arc::new(CanvasManager::new());
        let tools = canvas_tools(m.clone());

        // Find canvas.open_genome tool by name
        let open_genome = tools
            .iter()
            .find(|t| t.name() == "canvas.open_genome")
            .expect("canvas.open_genome tool must be registered");

        // Execute with canonical genome payload
        let result = open_genome
            .execute(
                json!({
                    "assembly": "hg38",
                    "region": "chr11:47354860-47395607",
                    "gene_symbol": "SPI1",
                    "title": "ATAC UCF hg38 — SPI1",
                    "samples": [{
                        "id": "UCF",
                        "label": "UCF",
                        "assay": "atac",
                        "uri": "gewu://sample/ucf_atac_hg38"
                    }],
                    "tracks": [{
                        "id": "signal-atac-UCF",
                        "type": "signal",
                        "source": "coverage",
                        "sample_id": "UCF",
                        "assay": "atac",
                        "uri": "gewu://track/ucf_atac_signal_hg38",
                        "path": "/data2/litian/macrophage_bk/data/ATAC/ucf/P1-2_ATAC3.filtered.ucf",
                        "url": "/data2/litian/macrophage_bk/data/ATAC/ucf/P1-2_ATAC3.filtered.ucf"
                    }]
                }),
                &ctx("s1"),
            )
            .await
            .unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();

        assert_eq!(output["event"], "canvas:open");
        assert_eq!(output["state"]["kind"], "genome");
        assert_eq!(output["state"]["payload"]["samples"][0]["id"], "UCF");
        assert_eq!(output["state"]["payload"]["tracks"][0]["sample_id"], "UCF");
    }

    #[tokio::test]
    async fn ucf_spi1_open_genome_workflow_is_canonical() {
        let m = Arc::new(CanvasManager::new());
        let tools = canvas_tools(m.clone());
        let open_genome = tools
            .iter()
            .find(|tool| tool.name() == "canvas.open_genome")
            .unwrap();
        let list = tools
            .iter()
            .find(|tool| tool.name() == "canvas.list")
            .unwrap();

        let opened = open_genome
            .execute(
                json!({
                    "assembly": "hg38",
                    "region": "chr11:47354860-47395607",
                    "gene_symbol": "SPI1",
                    "title": "ATAC UCF hg38 — SPI1",
                    "samples": [{
                        "id": "UCF",
                        "label": "UCF",
                        "assay": "atac",
                        "uri": "gewu://sample/ucf_atac_hg38"
                    }],
                    "tracks": [{
                        "id": "signal-atac-UCF",
                        "type": "signal",
                        "source": "coverage",
                        "sample_id": "UCF",
                        "assay": "atac",
                        "uri": "gewu://track/ucf_atac_signal_hg38",
                        "path": "/data2/litian/macrophage_bk/data/ATAC/ucf/P1-2_ATAC3.filtered.ucf",
                        "url": "/data2/litian/macrophage_bk/data/ATAC/ucf/P1-2_ATAC3.filtered.ucf"
                    }]
                }),
                &ctx("session-ucf"),
            )
            .await
            .unwrap();
        let opened_json: Value = serde_json::from_str(&opened.output).unwrap();
        assert_eq!(opened_json["event"], "canvas:open");

        let listed = list.execute(json!({}), &ctx("session-ucf")).await.unwrap();
        let listed_json: Value = serde_json::from_str(&listed.output).unwrap();
        assert_eq!(listed_json["canvases"].as_array().unwrap().len(), 1);
        assert_eq!(
            listed_json["canvases"][0]["payload"]["samples"][0]["id"],
            "UCF"
        );
        assert_eq!(
            listed_json["canvases"][0]["payload"]["tracks"][0]["sample_id"],
            "UCF"
        );
    }
}
