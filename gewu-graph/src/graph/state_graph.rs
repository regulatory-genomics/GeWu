use crate::graph::constants::{END, START};
use crate::state::State;
use futures::Stream;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::marker::Send as StdSend;
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc;

const MAX_SUBGRAPH_DEPTH: usize = 10;

/// Errors that can occur during graph execution.
#[derive(Error, Debug)]
pub enum GraphError {
    /// Returned when a node or edge references a node that doesn't exist.
    #[error("Node `{0}` not found")]
    NodeNotFound(String),

    /// Returned when invoking a graph that has no entry point set.
    #[error("No entry point set")]
    NoEntryPoint,

    /// Generic execution error containing a descriptive message.
    #[error("Execution error: {0}")]
    ExecutionError(String),

    /// Returned when nested subgraph depth exceeds the maximum allowed depth.
    /// This prevents infinite recursion in deeply nested subgraph structures.
    #[error("Subgraph depth limit ({0}) exceeded")]
    SubgraphDepthLimitExceeded(usize),
}

/// A tuple struct representing a message to be sent to another node.
///
/// The first element is the target node name, and the second is the state
/// to send to that node. When a node returns `NodeResult::Send`, the graph
/// will execute the target node with the provided state in parallel with
/// other operations.
#[derive(Clone, Debug)]
pub struct GraphSend<S: State>(pub String, pub S);

/// The result returned by a node's handler function.
///
/// NodeResult determines how the graph execution proceeds:
#[derive(Debug)]
pub enum NodeResult<S: State> {
    /// Update the current state with partial values.
    /// The graph will merge this partial state into the running state
    /// and continue execution along outgoing edges.
    Partial(S::Partial),

    /// Send the provided state to other nodes for parallel processing.
    /// The current node's execution ends here, and the targeted nodes
    /// will be executed with their respective sent states.
    Send(Vec<GraphSend<S>>),

    /// Emit custom data to any registered stream consumers.
    /// The state remains unchanged and execution continues along edges.
    Custom(serde_json::Value),
}

impl<S: State> NodeResult<S> {
    fn map_partial(partial: S::Partial) -> Self {
        NodeResult::Partial(partial)
    }
}

/// Controls what data is included in stream output from graph execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    /// Include the full state value at each step in the stream.
    Values,
    /// Include only the partial updates from each node in the stream.
    Updates,
    /// Include custom data emitted by nodes in the stream.
    Custom,
    /// Include task start/end events for each node execution.
    Tasks,
}

/// Output items from a streaming graph execution.
#[derive(Debug)]
pub enum StreamOutput<S: State> {
    /// The full state at some point in execution.
    Value(S),
    /// A partial state update from a node.
    Update {
        /// The name of the node that produced this update.
        node: String,
        /// The partial state update.
        update: S::Partial,
    },
    /// Custom data emitted by a node.
    Custom {
        /// The name of the node that emitted this data.
        node: String,
        /// The custom JSON data.
        data: serde_json::Value,
    },
    /// Marks the start of a node's execution (for tracking/debugging).
    TaskStart {
        /// Unique identifier for this task instance.
        id: String,
        /// Name of the node being executed.
        name: String,
    },
    /// Marks the end of a node's execution.
    TaskEnd {
        /// Unique identifier for this task instance.
        id: String,
        /// Name of the node that completed.
        name: String,
        /// The result from the node (typically default/empty).
        result: S::Partial,
    },
}

/// A writer for emitting custom data during streaming execution.
///
/// This is used internally when a node's handler needs to write custom
/// streaming data. It provides a sender that feeds into the stream output.
pub struct StreamWriter {
    sender: mpsc::UnboundedSender<serde_json::Value>,
}

impl StreamWriter {
    /// Write a JSON value to the stream.
    ///
    /// This will be emitted as a `StreamOutput::Custom` event with the
    /// node name automatically filled in by the graph executor.
    pub fn write(&self, data: serde_json::Value) {
        let _ = self.sender.send(data);
    }
}

/// Internal handler for a single node in the graph.
///
/// NodeHandler wraps the user-provided async function and optionally
/// holds a compiled subgraph for subgraph nodes.
#[derive(Clone)]
pub struct NodeHandler<S: State> {
    name: String,
    handler: Arc<dyn Fn(S) -> Pin<Box<dyn Future<Output = NodeResult<S>> + StdSend>> + Send + Sync>,
    subgraph: Option<Box<CompiledGraph<S>>>,
}

impl<S: State + std::fmt::Debug> std::fmt::Debug for NodeHandler<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeHandler")
            .field("name", &self.name)
            .field("has_subgraph", &self.subgraph.is_some())
            .finish()
    }
}

impl<S: State + 'static> NodeHandler<S> {
    /// Creates a new NodeHandler wrapping the given async function.
    pub fn new<F, Fut>(name: &str, handler: F) -> Self
    where
        F: Fn(S) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = NodeResult<S>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let wrapper: Arc<
            dyn Fn(S) -> Pin<Box<dyn Future<Output = NodeResult<S>> + StdSend>> + Send + Sync,
        > = Arc::new(move |state: S| {
            let h = handler.clone();
            Box::pin(async move { h(state).await })
                as Pin<Box<dyn Future<Output = NodeResult<S>> + StdSend>>
        });
        Self {
            name: name.to_string(),
            handler: wrapper,
            subgraph: None,
        }
    }

    /// Creates a NodeHandler that executes a compiled subgraph.
    ///
    /// When invoked, this handler will run the subgraph's entry point
    /// with the provided state and return the subgraph's final state
    /// as a partial update.
    pub fn with_subgraph(subgraph: CompiledGraph<S>) -> Self {
        let wrapper: Arc<
            dyn Fn(S) -> Pin<Box<dyn Future<Output = NodeResult<S>> + StdSend>> + Send + Sync,
        > = Arc::new(move |_: S| {
            Box::pin(async move { NodeResult::map_partial(S::Partial::default()) })
                as Pin<Box<dyn Future<Output = NodeResult<S>> + StdSend>>
        });
        Self {
            name: subgraph.entry_point.clone().unwrap_or_default(),
            handler: wrapper,
            subgraph: Some(Box::new(subgraph)),
        }
    }
}

/// A conditional edge that routes execution based on state values.
#[derive(Clone)]
pub struct ConditionalEdge<S: State> {
    path_fn: Arc<dyn Fn(&S) -> String + Send + Sync>,
    path_map: HashMap<String, String>,
}

impl<S: State + std::fmt::Debug> std::fmt::Debug for ConditionalEdge<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConditionalEdge")
            .field("path_map", &self.path_map)
            .finish()
    }
}

/// A builder for constructing stateful graphs with nodes, edges, and conditional routing.
///
/// StateGraph provides a fluent API for defining graph structure:
#[derive(Debug)]
pub struct StateGraph<S: State> {
    nodes: HashMap<String, NodeHandler<S>>,
    edges: HashSet<(String, String)>,
    conditional_edges: HashMap<String, Vec<ConditionalEdge<S>>>,
    entry_point: Option<String>,
    finish_points: HashSet<String>,
}

impl<S: State + 'static> Default for StateGraph<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: State + 'static> StateGraph<S> {
    /// Creates a new empty StateGraph with no nodes or edges.
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            edges: HashSet::new(),
            conditional_edges: HashMap::new(),
            entry_point: None,
            finish_points: HashSet::new(),
        }
    }

    /// Adds a node to the graph with the given name and async handler.
    ///
    /// The handler is an async function that receives the current state
    /// and returns a `NodeResult` that determines how execution proceeds.
    ///
    /// # Arguments
    /// * `name` - Unique identifier for this node
    /// * `handler` - Async function `(S) -> impl Future<Output = NodeResult<S>>`
    ///
    /// # Example
    ///
    /// ```
    /// # use gewu_graph::{StateGraph, State, NodeResult};
    /// # use serde::{Deserialize, Serialize};
    /// # #[derive(Debug, Clone, Default, Serialize, Deserialize)] struct S { count: i32 }
    /// # impl State for S { type Partial = S; fn merge(&self, _: Self::Partial) -> Self { self.clone() } }
    /// # fn p(count: i32) -> NodeResult<S> { NodeResult::Partial(S { count }) }
    /// let mut graph = StateGraph::<S>::new();
    ///
    /// graph.add_node("increment", |state: S| async move {
    ///     p(state.count + 1)
    /// });
    /// ```
    pub fn add_node<F, Fut>(&mut self, name: &str, handler: F) -> &mut Self
    where
        F: Fn(S) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = NodeResult<S>> + Send + 'static,
    {
        self.nodes
            .insert(name.to_string(), NodeHandler::new(name, handler));
        self
    }

    /// Adds a node that executes a compiled subgraph.
    ///
    /// When this node is executed, it will invoke the subgraph with the
    /// current state and use the subgraph's result as its output.
    ///
    /// Note: Subgraphs are NOT supported in `invoke_parallel()` and will panic.
    pub fn add_subgraph_node(&mut self, name: &str, subgraph: CompiledGraph<S>) -> &mut Self {
        let mut handler = NodeHandler::with_subgraph(subgraph);
        handler.name = name.to_string();
        self.nodes.insert(name.to_string(), handler);
        self
    }

    /// Adds a directed edge from one node to another.
    ///
    /// After `start` finishes execution, the graph will continue by
    /// executing `end`. Edges are evaluated after the source node
    /// returns, and all outgoing edges are followed.
    ///
    /// # Errors
    /// Returns `GraphError::ExecutionError` if `start` is `END` or `end` is `START`.
    pub fn add_edge(&mut self, start: &str, end: &str) -> Result<&mut Self, GraphError> {
        if start == END {
            return Err(GraphError::ExecutionError(
                "END cannot be used as edge start".into(),
            ));
        }
        if end == START {
            return Err(GraphError::ExecutionError(
                "START cannot be used as edge end".into(),
            ));
        }
        self.edges.insert((start.to_string(), end.to_string()));
        Ok(self)
    }

    /// Adds a conditional edge that routes execution based on the current state.
    ///
    /// The `path_fn` is called with the current state and should return a string
    /// representing which path to take. The `path_map` maps path results to
    /// destination node names. If the path function returns a key not in the
    /// map, that key is used directly as the destination node name.
    ///
    /// Multiple conditional edges can be added from the same source node;
    /// all will be evaluated and their destinations added to the execution queue.
    ///
    /// # Arguments
    /// * `source` - The node that will evaluate this condition
    /// * `path_fn` - Function `(S) -> String` that determines which path to take
    /// * `path_map` - Maps path results to destination node names
    ///
    /// # Example
    ///
    /// ```
    /// # use gewu_graph::{StateGraph, State, NodeResult, END};
    /// # use serde::{Deserialize, Serialize};
    /// # #[derive(Debug, Clone, Default, Serialize, Deserialize)] struct S { count: i32 }
    /// # impl State for S { type Partial = S; fn merge(&self, _: Self::Partial) -> Self { self.clone() } }
    /// # fn p(count: i32) -> NodeResult<S> { NodeResult::Partial(S { count }) }
    /// let mut graph = StateGraph::<S>::new();
    ///
    /// graph.add_node("check", |state: S| async move { p(state.count) });
    /// graph.add_node("high", |_: S| async { p(100) });
    /// graph.add_node("low", |_: S| async { p(0) });
    ///
    /// graph.set_entry_point("check");
    /// graph.add_conditional_edges(
    ///     "check",
    ///     |state: &S| if state.count >= 10 { "high".to_string() } else { "low".to_string() },
    ///     [("high".to_string(), "high".to_string())].into(),
    /// );
    /// graph.add_edge("low", END);
    /// graph.add_edge("high", END);
    /// ```
    pub fn add_conditional_edges(
        &mut self,
        source: &str,
        path_fn: impl Fn(&S) -> String + Send + Sync + 'static,
        path_map: HashMap<String, String>,
    ) -> &mut Self {
        let path_fn = Arc::new(path_fn);
        self.conditional_edges
            .entry(source.to_string())
            .or_default()
            .push(ConditionalEdge { path_fn, path_map });
        self
    }

    /// Sets the entry point for the graph.
    ///
    /// This is the first node that will be executed when the graph is invoked.
    /// Must be called before compiling.
    pub fn set_entry_point(&mut self, name: &str) -> &mut Self {
        self.entry_point = Some(name.to_string());
        self
    }

    /// Marks a node as a finish point.
    ///
    /// When a finish point node completes and there are no more nodes
    /// queued for execution, the graph returns the current state.
    /// Multiple finish points can be set.
    pub fn set_finish_point(&mut self, name: &str) -> &mut Self {
        self.finish_points.insert(name.to_string());
        self
    }

    /// Compiles the graph into an executable form.
    ///
    /// This creates a `CompiledGraph` that can be invoked multiple times.
    /// The compiled graph is cloneable and can be shared across threads.
    pub fn compile(&self) -> CompiledGraph<S> {
        CompiledGraph {
            nodes: self.nodes.clone(),
            edges: self.edges.clone(),
            conditional_edges: self.conditional_edges.clone(),
            entry_point: self.entry_point.clone(),
            finish_points: self.finish_points.clone(),
        }
    }
}

/// An compiled, executable graph ready to be invoked.
///
/// Created by calling `StateGraph::compile()`. A CompiledGraph can be
/// invoked multiple times with different inputs, and is safe to clone
/// and share across async tasks.
#[derive(Debug, Clone)]
pub struct CompiledGraph<S: State> {
    nodes: HashMap<String, NodeHandler<S>>,
    edges: HashSet<(String, String)>,
    conditional_edges: HashMap<String, Vec<ConditionalEdge<S>>>,
    entry_point: Option<String>,
    finish_points: HashSet<String>,
}

impl<S: State + 'static> CompiledGraph<S> {
    /// Synchronously invokes the graph with the given input state.
    ///
    /// This is a convenience method that wraps `ainvoke` with `block_on`.
    /// It will panic if called from within an async context.
    ///
    /// # Returns
    /// The final state after graph execution, or a `GraphError`.
    pub fn invoke(&self, input: S) -> Result<S, GraphError> {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(self.ainvoke(input))
    }

    /// Asynchronously invokes the graph with the given input state.
    ///
    /// This is the main entry point for graph execution. The graph will
    /// start at the entry point and follow edges based on node results
    /// until reaching a finish point (or running out of nodes to execute).
    ///
    /// # Returns
    /// The final state after graph execution, or a `GraphError`.
    pub async fn ainvoke(&self, input: S) -> Result<S, GraphError> {
        self.ainvoke_internal(input, 0).await
    }

    async fn ainvoke_internal(&self, input: S, depth: usize) -> Result<S, GraphError> {
        if depth > MAX_SUBGRAPH_DEPTH {
            return Err(GraphError::SubgraphDepthLimitExceeded(MAX_SUBGRAPH_DEPTH));
        }

        let entry = match self.entry_point.clone() {
            Some(e) => e,
            None => return Err(GraphError::NoEntryPoint),
        };

        let mut current_state = input;
        let mut to_execute: Vec<String> = vec![entry];
        let mut sent_to: HashSet<String> = HashSet::new();

        while let Some(node_name) = to_execute.pop() {
            let handler = self
                .nodes
                .get(&node_name)
                .ok_or_else(|| GraphError::NodeNotFound(node_name.clone()))?;

            if let Some(subgraph) = &handler.subgraph {
                let subgraph = subgraph.clone();
                let new_depth = depth + 1;
                current_state =
                    Box::pin(
                        async move { subgraph.ainvoke_internal(current_state, new_depth).await },
                    )
                    .await?;
            } else {
                let result = (handler.handler)(current_state.clone()).await;
                let is_send = matches!(result, NodeResult::Send(_));
                current_state = match result {
                    NodeResult::Partial(partial) => current_state.merge(partial),
                    NodeResult::Send(sends) => {
                        let sent_names: Vec<String> = sends.iter().map(|s| s.0.clone()).collect();
                        for name in &sent_names {
                            sent_to.insert(name.clone());
                        }
                        current_state = self.process_sends(sends, current_state, depth).await?;
                        current_state
                    }
                    NodeResult::Custom(_) => current_state,
                };
                if is_send {
                    continue;
                }
            }

            let outgoing = self.get_outgoing_edges(&node_name, &current_state);
            for edge in outgoing {
                if edge == END || edge.is_empty() || sent_to.contains(&edge) {
                    continue;
                }
                to_execute.push(edge);
            }

            if self.finish_points.contains(&node_name) && to_execute.is_empty() {
                return Ok(current_state);
            }
        }

        Ok(current_state)
    }

    /// Processes Send operations, executing target nodes in parallel.
    async fn process_sends(
        &self,
        sends: Vec<GraphSend<S>>,
        mut current_state: S,
        depth: usize,
    ) -> Result<S, GraphError> {
        if sends.is_empty() {
            return Ok(current_state);
        }

        #[derive(Debug)]
        enum SendResult<S: State> {
            Partial(NodeResult<S>),
            Subgraph(Box<CompiledGraph<S>>, S),
        }

        let mut futures: Vec<_> = Vec::new();

        for send in sends {
            let handler = self.nodes.get(&send.0).cloned();
            let state_to_use = send.1;

            futures.push(async move {
                match handler {
                    Some(h) if h.subgraph.is_none() => {
                        let result = (h.handler)(state_to_use).await;
                        Ok(SendResult::Partial(result))
                    }
                    Some(h) if h.subgraph.is_some() => {
                        let subgraph = h.subgraph.clone().unwrap();
                        Ok(SendResult::Subgraph(subgraph, state_to_use))
                    }
                    _ => Err(GraphError::NodeNotFound(send.0.clone())),
                }
            });
        }

        let results: Vec<Result<SendResult<S>, GraphError>> =
            futures::future::join_all(futures).await;

        for result in results {
            match result {
                Ok(SendResult::Partial(node_result)) => match node_result {
                    NodeResult::Partial(partial) => {
                        current_state = current_state.merge(partial);
                    }
                    NodeResult::Send(nested_sends) => {
                        let nested_sends = nested_sends;
                        let this = self;
                        let d = depth + 1;
                        current_state = Box::pin(async move {
                            this.process_sends(nested_sends, current_state, d).await
                        })
                        .await?;
                    }
                    NodeResult::Custom(_) => {}
                },
                Ok(SendResult::Subgraph(subgraph, state)) => {
                    current_state =
                        Box::pin(async { subgraph.ainvoke_internal(state, depth + 1).await })
                            .await?;
                }
                Err(e) => return Err(e),
            }
        }

        Ok(current_state)
    }

    /// Returns a stream of state values as the graph executes.
    ///
    /// This is equivalent to `stream_with_modes(input, vec![StreamMode::Values])`.
    /// The stream yields `StreamOutput::Value` items representing the state
    /// at each step of execution.
    pub fn stream(&self, input: S) -> impl Stream<Item = Result<StreamOutput<S>, GraphError>> + '_ {
        self.stream_with_modes(input, vec![StreamMode::Values])
    }

    /// Returns a stream with configurable output modes.
    ///
    /// Allows selective output of different event types via `StreamMode`.
    /// Multiple modes can be combined to get comprehensive streaming data.
    ///
    /// # Arguments
    /// * `input` - Initial state to pass to the graph
    /// * `modes` - Vector of `StreamMode` variants to include in output
    ///
    /// # Example
    ///
    /// ```
    /// # use gewu_graph::{CompiledGraph, State, StreamMode, StreamOutput};
    /// # use futures::StreamExt;
    /// # use serde::{Deserialize, Serialize};
    /// # #[derive(Debug, Clone, Default, Serialize, Deserialize)] struct S { count: i32 }
    /// # impl State for S { type Partial = S; fn merge(&self, _: Self::Partial) -> Self { self.clone() } }
    /// # async fn example(graph: CompiledGraph<S>) {
    /// let stream = graph.stream_with_modes(
    ///     S { count: 0 },
    ///     vec![StreamMode::Values, StreamMode::Updates, StreamMode::Tasks],
    /// );
    /// // stream yields StreamOutput::Value, StreamOutput::Update, and StreamOutput::TaskStart/End
    /// # }
    /// ```
    pub fn stream_with_modes(
        &self,
        input: S,
        modes: Vec<StreamMode>,
    ) -> impl Stream<Item = Result<StreamOutput<S>, GraphError>> + '_ {
        async_stream::try_stream! {
            let entry = self.entry_point.clone().ok_or(GraphError::NoEntryPoint)?;

            let mut current_state = input;
            let mut to_execute: Vec<String> = vec![entry];
            let has_values = modes.contains(&StreamMode::Values);
            let has_updates = modes.contains(&StreamMode::Updates);
            let has_custom = modes.contains(&StreamMode::Custom);
            let has_tasks = modes.contains(&StreamMode::Tasks);

            if has_values {
                yield StreamOutput::Value(current_state.clone());
            }

            while let Some(node_name) = to_execute.pop() {
                let handler = self
                    .nodes
                    .get(&node_name)
                    .ok_or_else(|| GraphError::NodeNotFound(node_name.clone()))?;

                let task_id = format!("{}_{}", node_name, uuid::Uuid::new_v4());

                if has_tasks {
                    yield StreamOutput::TaskStart {
                        id: task_id.clone(),
                        name: node_name.clone(),
                    };
                }

                let node_result = if let Some(subgraph) = &handler.subgraph {
                    let new_state = subgraph.ainvoke(current_state.clone()).await?;
                    current_state = new_state;
                    NodeResult::Partial(S::Partial::default())
                } else {
                    (handler.handler)(current_state.clone()).await
                };

                let is_send = matches!(node_result, NodeResult::Send(_));
                match node_result {
                    NodeResult::Partial(partial) => {
                        if has_updates {
                            yield StreamOutput::Update {
                                node: node_name.clone(),
                                update: partial.clone(),
                            };
                        }
                        current_state = current_state.merge(partial);
                    }
                    NodeResult::Send(sends) => {
                        current_state = self.process_sends(sends, current_state, 0).await?;
                    }
                    NodeResult::Custom(data) => {
                        if has_custom {
                            yield StreamOutput::Custom {
                                node: node_name.clone(),
                                data,
                            };
                        }
                    }
                }

                if has_tasks {
                    yield StreamOutput::TaskEnd {
                        id: task_id.clone(),
                        name: node_name.clone(),
                        result: S::Partial::default(),
                    };
                }

                if has_values {
                    yield StreamOutput::Value(current_state.clone());
                }

                if is_send {
                    continue;
                }

                let outgoing = self.get_outgoing_edges(&node_name, &current_state);
                for edge in outgoing {
                    if edge == END || edge.is_empty() {
                        continue;
                    }
                    to_execute.push(edge);
                }

                if self.finish_points.contains(&node_name) && to_execute.is_empty() {
                    break;
                }
            }
        }
    }

    /// Returns an async stream of state values only.
    ///
    /// This is a convenience method that filters the stream to only
    /// yield `StreamOutput::Value` items, ignoring updates and other events.
    pub async fn astream(&self, input: S) -> impl Stream<Item = Result<S, GraphError>> + '_ {
        self.stream(input).filter_map(|r| async move {
            match r {
                Ok(StreamOutput::Value(v)) => Some(Ok(v)),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            }
        })
    }

    /// Executes nodes in a Bulk Synchronous Parallel (BSP) style.
    ///
    /// All nodes in the current "superstep" are executed in parallel.
    /// After all nodes in a superstep complete, the next superstep begins
    /// with nodes that were targeted by Send operations or regular edges.
    ///
    /// This mode is useful for graphs where multiple independent branches
    /// can be computed concurrently, with state merges happening between
    /// supersteps.
    ///
    /// # Errors
    /// Returns `GraphError::ExecutionError` if the graph contains subgraph nodes.
    pub async fn invoke_parallel(&self, input: S) -> Result<S, GraphError> {
        let entry = self.entry_point.clone().ok_or(GraphError::NoEntryPoint)?;

        let mut current_state = input;
        let mut finished: HashSet<String> = HashSet::new();
        let mut ready: Vec<String> = vec![entry];

        while !ready.is_empty() {
            let superstep_state = current_state.clone();

            let mut futures: Vec<_> = Vec::new();

            for name in ready.iter() {
                if finished.contains(name) {
                    continue;
                }
                if let Some(handler) = self.nodes.get(name) {
                    let state_clone = superstep_state.clone();
                    let has_subgraph = handler.subgraph.is_some();

                    futures.push(async move {
                        if has_subgraph {
                            panic!("Subgraphs are not supported in invoke_parallel");
                        }
                        let result = (handler.handler)(state_clone).await;
                        (name.clone(), result)
                    });
                }
            }

            if futures.is_empty() {
                break;
            }

            let results: Vec<(String, NodeResult<S>)> = futures::future::join_all(futures).await;

            let mut next_ready: Vec<String> = Vec::new();

            for (name, result) in results {
                let is_send = matches!(result, NodeResult::Send(_));
                match result {
                    NodeResult::Partial(partial) => {
                        current_state = current_state.merge(partial);
                    }
                    NodeResult::Send(sends) => {
                        for send in sends {
                            if !finished.contains(&send.0) && !next_ready.contains(&send.0) {
                                next_ready.push(send.0.clone());
                            }
                        }
                    }
                    NodeResult::Custom(_) => {}
                }
                finished.insert(name.clone());

                if is_send {
                    continue;
                }

                let outgoing = self.get_outgoing_edges(&name, &current_state);
                for edge in outgoing {
                    if edge == END || edge.is_empty() {
                        continue;
                    }
                    if !finished.contains(&edge) && !next_ready.contains(&edge) {
                        next_ready.push(edge);
                    }
                }
            }

            ready = next_ready;
        }

        Ok(current_state)
    }

    /// Determines outgoing edges for a node given current state.
    ///
    /// Combines regular edges with conditional edges, evaluating all
    /// conditions to build the list of nodes to execute next.
    fn get_outgoing_edges(&self, node_name: &str, state: &S) -> Vec<String> {
        let mut outgoing = Vec::new();

        for (start, end) in &self.edges {
            if start == node_name {
                outgoing.push(end.clone());
            }
        }

        if let Some(conditions) = self.conditional_edges.get(node_name) {
            for cond in conditions {
                let path_result = (cond.path_fn)(state);
                let dest = cond
                    .path_map
                    .get(&path_result)
                    .cloned()
                    .unwrap_or(path_result);
                outgoing.push(dest);
            }
        }

        outgoing
    }
}
