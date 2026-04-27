//! Graph execution core components.
//!
//! This module contains the main graph building and execution primitives:
//!
//! - `StateGraph` - Builder for constructing graphs
//! - `CompiledGraph` - Executable compiled graph
//! - `GraphTool` - Adapter for using graphs as Tools
//! - Supporting types: `GraphError`, `NodeResult`, `StreamMode`, etc.

pub mod constants;
pub mod state_graph;
pub mod tool;

pub use constants::{END, START};
pub use state_graph::{
    CompiledGraph, GraphError, GraphSend, NodeResult, StateGraph, StreamMode, StreamOutput,
    StreamWriter,
};
pub use tool::GraphTool;
