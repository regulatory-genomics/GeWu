//! Six `Tool` implementations exposing canvas operations to the agent.
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
use serde_json::{json, Value};

use crate::manager::CanvasManager;
use crate::state::{CanvasEvent, CanvasKind, CanvasMode, CanvasState, CanvasStatePatch};

/// Build the canonical six canvas tools, sharing a single manager.
pub fn canvas_tools(manager: Arc<CanvasManager>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(CanvasOpenTool {
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
         The canvas appears as a wide overlay in the user's browser. Use kind=\"genome\" for a \
         genomic browser canvas. The canvas is per-session; use canvas.list to inspect what \
         is open and canvas.get_state to read what the user is currently viewing."
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
                    "description": "Display mode (default overlay)."
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
        let preview = preview_canvas_state(&parsed);
        let event = CanvasEvent::Open {
            canvas_id: preview.canvas_id.clone(),
            kind: preview.kind,
            mode: preview.mode,
            initial_state: preview,
        };
        Ok(serde_json::to_value(event).map_err(box_err)?)
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: OpenArgs = serde_json::from_value(args).map_err(box_err)?;
        let mode = input.mode.unwrap_or_default();
        let state = self
            .manager
            .open(
                &ctx.session_id,
                input.canvas_id.clone(),
                input.kind,
                mode,
                input.title,
                input.initial_state,
            )
            .await;

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

fn preview_canvas_state(args: &OpenArgs) -> CanvasState {
    let id = args.canvas_id.clone().unwrap_or_else(|| "pending".into());
    let mut state = CanvasState::new(id, args.kind);
    state.mode = args.mode.unwrap_or_default();
    state.title = args.title.clone().filter(|t| !t.is_empty());
    if let Some(payload) = &args.initial_state {
        state.payload = payload.clone();
    }
    state
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
            Ok(()) => Ok(ToolResult::new(
                "canvas.close",
                json!({
                    "canvas_id": input.canvas_id,
                    "closed": true,
                    "event": "canvas:close",
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
            Err(e) => Ok(ToolResult::new(
                "canvas.set_state",
                json!({
                    "error": { "code": "canvas_not_found", "message": e.to_string() }
                })
                .to_string(),
            )),
        }
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
        let state = self.manager.get_state(&ctx.session_id, &input.canvas_id).await;
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

    #[tokio::test]
    async fn open_then_get_state_round_trip() {
        let m = Arc::new(CanvasManager::new());
        let tools = canvas_tools(m.clone());
        let open = &tools[0];
        let get = &tools[4];

        let opened_raw = open
            .execute(
                json!({ "kind": "genome", "initial_state": { "region": "chr1:1-100" } }),
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
    async fn start_metadata_emits_canvas_open_event() {
        let m = Arc::new(CanvasManager::new());
        let open = CanvasOpenTool {
            manager: m.clone(),
        };
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
                Some(json!({"region": "chr1:1-100"})),
            )
            .await;
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
        m.open("s1", None, CanvasKind::Genome, CanvasMode::Overlay, None, None)
            .await;
        m.open("s2", None, CanvasKind::Genome, CanvasMode::Overlay, None, None)
            .await;
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
}
