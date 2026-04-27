//! # gewu-graph
//!
//! A stateful graph execution library for building complex workflow orchestration
//! with support for conditional routing, parallel execution, and nested subgraphs.
//!
//! ## Core Concepts
//!
//! ### State
//!
//! Graphs operate on a shared `State` that flows through nodes. Your state must
//! implement the `State` trait which defines how partial updates are merged:
//!
//! ```
//! use gewu_graph::{State, NodeResult};
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Debug, Clone, Default, Serialize, Deserialize)]
//! struct MyState {
//!     count: i32,
//!     messages: Vec<String>,
//! }
//!
//! #[derive(Debug, Clone, Default, Serialize, Deserialize)]
//! struct PartialMyState {
//!     count: Option<i32>,
//!     messages: Option<Vec<String>>,
//! }
//!
//! impl State for MyState {
//!     type Partial = PartialMyState;
//!
//!     fn merge(&self, update: Self::Partial) -> Self {
//!         Self {
//!             count: update.count.unwrap_or(self.count),
//!             messages: update.messages.unwrap_or_else(|| self.messages.clone()),
//!         }
//!     }
//! }
//! ```
//!
//! ### Building Graphs
//!
//! Use `StateGraph` to construct your workflow:
//!
//! ```
//! # use gewu_graph::{StateGraph, State, NodeResult, END};
//! # use serde::{Deserialize, Serialize};
//! # #[derive(Debug, Clone, Default, Serialize, Deserialize)] struct S { count: i32 }
//! # impl State for S { type Partial = S; fn merge(&self, _: Self::Partial) -> Self { self.clone() } }
//! # fn p(count: i32) -> NodeResult<S> { NodeResult::Partial(S { count }) }
//! let mut graph = StateGraph::<S>::new();
//!
//! // Add nodes with async handler functions
//! graph.add_node("increment", |state: S| async move {
//!     p(state.count + 1)
//! });
//!
//! graph.set_entry_point("increment");
//! graph.add_edge("increment", END);
//! # let _ = graph.compile();
//! ```
//!
//! ### Execution Modes
//!
//! - `invoke()` - Synchronous execution (blocks)
//! - `ainvoke()` - Async execution
//! - `stream()` / `astream()` - Streaming state updates
//! - `invoke_parallel()` - Bulk Synchronous Parallel (BSP) execution
//!
//! ### Node Results
//!
//! Nodes return `NodeResult<S>` which can be:
//! - `Partial(partial_state)` - Update the state with partial values
//! - `Send(targets)` - Send state to other nodes for parallel processing
//! - `Custom(json_value)` - Emit custom data in streams without affecting state

pub mod channels;
pub mod graph;
pub mod state;
#[cfg(test)]
mod tests;

pub use channels::{
    AnyValueChannel, BaseChannel, BinaryOperatorChannel, EphemeralValueChannel, LastValueChannel,
    NamedBarrierChannel, TopicChannel,
};
pub use graph::{
    CompiledGraph, GraphError, GraphSend, GraphTool, NodeResult, StateGraph, StreamMode,
    StreamOutput, StreamWriter,
};
pub use state::{Append, Blackboard, LastValue, Reducer, State, Sum};

/// The reserved name for the graph's entry point.
///
/// Use this constant when adding edges that should execute first.
/// When a node returns `NodeResult::Send`, the sent state flows to the
/// specified target nodes. When a node finishes and has an edge to `START`,
/// that edge is followed to continue execution.
pub const START: &str = "__start__";

/// The reserved name marking the end of graph execution.
///
/// When execution reaches a node with an edge to `END`, or when all
/// paths have completed and there are no more nodes to execute,
/// the graph execution terminates and returns the final state.
pub const END: &str = "__end__";
