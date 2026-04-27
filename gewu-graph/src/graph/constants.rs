//! Reserved constants for graph construction.
//!
//! These constants are used as special node names in graph definitions.

/// The reserved identifier for the entry point of a graph.
///
/// Used internally to mark where execution begins. When adding edges,
/// you typically don't use `START` directly as a source node name.
/// The entry point is set via `StateGraph::set_entry_point()`.
pub const START: &str = "__start__";

/// The reserved identifier marking the end of graph execution.
///
/// When a node has an outgoing edge to `END`, execution terminates
/// at that point. Multiple edges to `END` can be added to create
/// different exit points in the graph.
pub const END: &str = "__end__";
