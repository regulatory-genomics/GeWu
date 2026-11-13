//! Dynamic workflows: the LLM writes a JSON graph spec, the engine
//! compiles it onto a `gewu-graph` `StateGraph` and runs it in the
//! background.
//!
//! - [`spec`] — the `WorkflowSpec` types and pre-spawn validation
//! - [`template`] — the fixed prompt-template vocabulary
//! - [`graph_state`] — the state that flows between compiled nodes
//! - [`engine`] — compiles and runs one workflow (fan-out, routing,
//!   review/redo, limits)
//! - [`manager`] — tracks background runs (`WorkflowManager`)
//! - [`tools`] — `workflow_start` / `workflow_status` / `workflow_wait` /
//!   `workflow_cancel`

pub mod engine;
pub mod graph_state;
pub mod manager;
mod planning;
pub mod spec;
pub mod template;
#[cfg(test)]
pub(crate) mod testutil;
pub mod tools;

pub use manager::WorkflowManager;
pub use tools::{WorkflowCancelTool, WorkflowStartTool, WorkflowStatusTool, WorkflowWaitTool};
