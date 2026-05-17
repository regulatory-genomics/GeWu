//! # gewu-canvas
//!
//! Generic agent-controllable canvas runtime for GeWu.
//!
//! This crate is the GeWu side of the tachyon-canvas / GenomeCanvas system.
//! It owns:
//!
//!   - the [`CanvasState`] / [`GenomeCanvasState`] / [`CanvasStatePatch`] Rust
//!     types (mirrors of `tachyon-canvas/contracts/*.ts`),
//!   - the in-memory [`CanvasManager`] that holds per-session canvases,
//!   - six [`gewu_core::traits::Tool`] implementations
//!     (`canvas.open / close / focus / set_state / get_state / list`).
//!
//! It is deliberately kind-agnostic: the genome canvas frontend, evidence
//! contracts, and tachyon-browser integration all live in the
//! `tachyon-canvas` repo. GeWu only knows that a canvas of `kind = "genome"`
//! exists and that its `payload` is opaque JSON.
//!
//! ## SSE bridge
//!
//! Per `tachyon-canvas/docs/sse-bridge.md`, canvas events ride on the
//! existing GeWu tool-event stream:
//!
//!   - [`tools::CanvasOpenTool::start_metadata`] returns
//!     `{"event": "canvas:open", ...}` which the chat handler forwards into
//!     `tool_start.metadata`.
//!   - [`tools::CanvasSetStateTool::execute`] returns a `ToolResult` whose
//!     `output` is JSON containing `{"event": "canvas:patch", ...}` — already
//!     parsed by the existing frontend `parseToolOutputJson`.
//!
//! No changes to `gewu_core::types::AgentEvent` are required.

pub mod manager;
pub mod observation;
pub mod state;
pub mod subscription;
pub mod tools;
pub mod workflow_emit;

pub use manager::{CanvasError, CanvasManager, CanvasValidationIssue};
pub use state::{
    CANVAS_STATE_VERSION, CanvasEvent, CanvasKind, CanvasMode, CanvasState, CanvasStatePatch,
    GenomeCanvasOpenRequest, GenomeCanvasSampleInput, GenomeCanvasState, GenomeCanvasTrackInput,
    GenomeSelection, GenomeViewport, SampleSpec, TrackSpec, TrackType, build_genome_canvas_payload,
};
pub use tools::canvas_tools;
pub use workflow_emit::WorkflowEmitTool;
