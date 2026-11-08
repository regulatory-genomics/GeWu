use crate::graph::constants::{END, START};
use crate::state::State;
use futures::Stream;
use futures::StreamExt;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::marker::Send as StdSend;
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc;

const MAX_SUBGRAPH_DEPTH: usize = 10;
const DEFAULT_MAX_STEPS: usize = 1000;

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

    /// Returned when execution exceeds the configured number of supersteps.
    #[error("Execution step limit ({0}) exceeded")]
    StepLimitExceeded(usize),

    /// A node returned a structured failure. Downstream nodes did not run.
    #[error("Node `{node}` failed ({code}): {message}")]
    NodeFailed {
        node: String,
        code: String,
        message: String,
        retryable: bool,
    },
}

/// A tuple struct representing a message to be sent to another node.
///
/// The first element is the target node name, and the second is the state
/// to send to that node. When a node returns `NodeResult::Send`, the graph
/// will execute the target node with the provided state in parallel with
/// other operations.
#[derive(Clone, Debug)]
pub struct GraphSend<S: State>(pub String, pub S);

/// A structured node failure.
///
/// Carries a machine-readable code so a caller can decide whether to retry,
/// and never conflates a failed step with a successful empty result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeError {
    /// Stable error code, for example `BACKEND_UNAVAILABLE`.
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
    /// Whether repeating the same input could succeed.
    pub retryable: bool,
}

impl NodeError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }
}

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

    /// The node failed.
    ///
    /// Successful peers in the same superstep keep their emitted events, but
    /// nothing downstream of the failure is scheduled and the graph returns
    /// [`GraphError::NodeFailed`].
    Fail(NodeError),
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
    /// Marks a node's structured failure.
    TaskFailed {
        /// Unique identifier for this task instance.
        id: String,
        /// Name of the node that failed.
        name: String,
        /// Typed failure returned by the node.
        error: NodeError,
    },
}

#[derive(Clone, Debug)]
struct WaitingEdge {
    sources: HashSet<String>,
    target: String,
}

#[derive(Clone, Debug)]
struct ScheduledTask<S: State> {
    id: String,
    node: String,
    input: S,
    path: Vec<usize>,
}

#[derive(Debug)]
enum TaskResult<S: State> {
    Node(NodeResult<S>),
    Subgraph(S),
}

#[derive(Debug)]
struct TaskOutcome<S: State> {
    task: ScheduledTask<S>,
    result: TaskResult<S>,
}

#[derive(Debug)]
enum ExecutionEvent<S: State> {
    Value(S),
    Update {
        node: String,
        update: S::Partial,
    },
    Custom {
        node: String,
        data: serde_json::Value,
    },
    TaskStart {
        id: String,
        name: String,
    },
    TaskEnd {
        id: String,
        name: String,
        result: S::Partial,
    },
    TaskFailed {
        id: String,
        name: String,
        error: NodeError,
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
    waiting_edges: Vec<WaitingEdge>,
    max_steps: usize,
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
            waiting_edges: Vec::new(),
            max_steps: DEFAULT_MAX_STEPS,
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
    /// # Panics
    /// Panics if `start` is `START` or `end` is `END`.
    pub fn add_edge(&mut self, start: &str, end: &str) -> &mut Self {
        if start == END {
            panic!("START cannot be END node");
        }
        if end == START {
            panic!("END cannot be START node");
        }
        self.edges.insert((start.to_string(), end.to_string()));
        self
    }

    /// Adds an edge that triggers `target` only after every named source has completed.
    pub fn add_waiting_edge<I, T>(&mut self, sources: I, target: &str) -> &mut Self
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        let sources = sources.into_iter().map(Into::into).collect::<HashSet<_>>();
        assert!(
            !sources.is_empty(),
            "Waiting edge requires at least one source"
        );
        assert!(target != START, "Waiting edge target cannot be START");
        self.waiting_edges.push(WaitingEdge {
            sources,
            target: target.to_string(),
        });
        self
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

    /// Sets the maximum number of execution supersteps.
    pub fn set_max_steps(&mut self, max_steps: usize) -> &mut Self {
        self.max_steps = max_steps;
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
            waiting_edges: self.waiting_edges.clone(),
            max_steps: self.max_steps,
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
    waiting_edges: Vec<WaitingEdge>,
    max_steps: usize,
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
        self.execute_with_sink(input, depth, |_| {}).await
    }

    async fn execute_with_sink<F>(
        &self,
        input: S,
        depth: usize,
        mut emit: F,
    ) -> Result<S, GraphError>
    where
        F: FnMut(ExecutionEvent<S>) + Send,
    {
        if depth > MAX_SUBGRAPH_DEPTH {
            return Err(GraphError::SubgraphDepthLimitExceeded(MAX_SUBGRAPH_DEPTH));
        }

        let entry = self.entry_point.clone().ok_or(GraphError::NoEntryPoint)?;
        let mut current_state = input;
        let mut tasks = vec![ScheduledTask {
            id: uuid::Uuid::new_v4().to_string(),
            node: entry,
            input: current_state.clone(),
            path: vec![0],
        }];
        let mut barrier_arrivals = vec![HashSet::<String>::new(); self.waiting_edges.len()];
        let mut step = 0;

        emit(ExecutionEvent::Value(current_state.clone()));

        while !tasks.is_empty() {
            if step >= self.max_steps {
                return Err(GraphError::StepLimitExceeded(self.max_steps));
            }
            step += 1;
            tasks.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.node.cmp(&b.node)));

            let mut futures = Vec::with_capacity(tasks.len());
            let current_tasks = std::mem::take(&mut tasks);
            for task in current_tasks {
                let handler = self
                    .nodes
                    .get(&task.node)
                    .cloned()
                    .ok_or_else(|| GraphError::NodeNotFound(task.node.clone()))?;
                emit(ExecutionEvent::TaskStart {
                    id: task.id.clone(),
                    name: task.node.clone(),
                });

                futures.push(async move {
                    let result = if let Some(subgraph) = handler.subgraph {
                        let state =
                            Box::pin(subgraph.ainvoke_internal(task.input.clone(), depth + 1))
                                .await?;
                        TaskResult::Subgraph(state)
                    } else {
                        TaskResult::Node((handler.handler)(task.input.clone()).await)
                    };
                    Ok::<_, GraphError>(TaskOutcome { task, result })
                });
            }

            let outcomes = futures::future::join_all(futures)
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            struct CompletedTask<S: State> {
                task: ScheduledTask<S>,
                local_state: S,
                sends: Option<Vec<GraphSend<S>>>,
            }

            let mut updates = Vec::new();
            let mut branches = Vec::new();
            let mut completed = Vec::new();
            let mut failures: Vec<(String, NodeError)> = Vec::new();

            for outcome in outcomes {
                let task = outcome.task;
                match outcome.result {
                    TaskResult::Node(NodeResult::Partial(update)) => {
                        let local_state = task.input.merge(update.clone());
                        emit(ExecutionEvent::Update {
                            node: task.node.clone(),
                            update: update.clone(),
                        });
                        emit(ExecutionEvent::TaskEnd {
                            id: task.id.clone(),
                            name: task.node.clone(),
                            result: update.clone(),
                        });
                        updates.push(update);
                        completed.push(CompletedTask {
                            task,
                            local_state,
                            sends: None,
                        });
                    }
                    TaskResult::Node(NodeResult::Send(sends)) => {
                        emit(ExecutionEvent::TaskEnd {
                            id: task.id.clone(),
                            name: task.node.clone(),
                            result: S::Partial::default(),
                        });
                        completed.push(CompletedTask {
                            local_state: task.input.clone(),
                            task,
                            sends: Some(sends),
                        });
                    }
                    TaskResult::Node(NodeResult::Custom(data)) => {
                        emit(ExecutionEvent::Custom {
                            node: task.node.clone(),
                            data,
                        });
                        emit(ExecutionEvent::TaskEnd {
                            id: task.id.clone(),
                            name: task.node.clone(),
                            result: S::Partial::default(),
                        });
                        completed.push(CompletedTask {
                            local_state: task.input.clone(),
                            task,
                            sends: None,
                        });
                    }
                    TaskResult::Subgraph(branch) => {
                        emit(ExecutionEvent::TaskEnd {
                            id: task.id.clone(),
                            name: task.node.clone(),
                            result: S::Partial::default(),
                        });
                        branches.push(branch.clone());
                        completed.push(CompletedTask {
                            task,
                            local_state: branch,
                            sends: None,
                        });
                    }
                    TaskResult::Node(NodeResult::Fail(error)) => {
                        emit(ExecutionEvent::TaskFailed {
                            id: task.id.clone(),
                            name: task.node.clone(),
                            error: error.clone(),
                        });
                        failures.push((task.node, error));
                    }
                }
            }

            // Peers that already finished in this superstep keep their events,
            // but a failed node never schedules its downstream.
            if let Some((node, error)) = failures.into_iter().next() {
                return Err(GraphError::NodeFailed {
                    node,
                    code: error.code,
                    message: error.message,
                    retryable: error.retryable,
                });
            }

            current_state = current_state.merge_many(updates);
            for branch in branches {
                current_state = current_state.merge_branch(branch);
            }
            emit(ExecutionEvent::Value(current_state.clone()));

            let mut push_tasks = Vec::new();
            let mut pull_targets = BTreeMap::<String, Vec<usize>>::new();

            for completed_task in completed {
                if let Some(sends) = completed_task.sends {
                    for (index, GraphSend(target, input)) in sends.into_iter().enumerate() {
                        if !self.nodes.contains_key(&target) {
                            return Err(GraphError::NodeNotFound(target));
                        }
                        let mut path = completed_task.task.path.clone();
                        path.push(index);
                        push_tasks.push(ScheduledTask {
                            id: uuid::Uuid::new_v4().to_string(),
                            node: target,
                            input,
                            path,
                        });
                    }
                    continue;
                }

                for (index, target) in self
                    .get_outgoing_edges(&completed_task.task.node, &completed_task.local_state)
                    .into_iter()
                    .enumerate()
                {
                    if target == END || target.is_empty() {
                        continue;
                    }
                    let mut path = completed_task.task.path.clone();
                    path.push(index);
                    pull_targets.entry(target).or_insert(path);
                }

                for (index, waiting_edge) in self.waiting_edges.iter().enumerate() {
                    if waiting_edge.sources.contains(&completed_task.task.node) {
                        barrier_arrivals[index].insert(completed_task.task.node.clone());
                        if barrier_arrivals[index] == waiting_edge.sources {
                            pull_targets
                                .entry(waiting_edge.target.clone())
                                .or_insert_with(|| completed_task.task.path.clone());
                            barrier_arrivals[index].clear();
                        }
                    }
                }
            }

            for (node, path) in pull_targets {
                tasks.push(ScheduledTask {
                    id: uuid::Uuid::new_v4().to_string(),
                    node,
                    input: current_state.clone(),
                    path,
                });
            }
            tasks.extend(push_tasks);
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
        let graph = self.clone();
        async_stream::stream! {
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(async move {
                let result = graph
                    .execute_with_sink(input, 0, |event| {
                        let _ = sender.send(Ok(event));
                    })
                    .await;
                if let Err(error) = result {
                    let _ = sender.send(Err(error));
                }
            });

            while let Some(event) = receiver.recv().await {
                match event {
                    Ok(ExecutionEvent::Value(state)) if modes.contains(&StreamMode::Values) => {
                        yield Ok(StreamOutput::Value(state));
                    }
                    Ok(ExecutionEvent::Update { node, update })
                        if modes.contains(&StreamMode::Updates) =>
                    {
                        yield Ok(StreamOutput::Update { node, update });
                    }
                    Ok(ExecutionEvent::Custom { node, data })
                        if modes.contains(&StreamMode::Custom) =>
                    {
                        yield Ok(StreamOutput::Custom { node, data });
                    }
                    Ok(ExecutionEvent::TaskStart { id, name })
                        if modes.contains(&StreamMode::Tasks) =>
                    {
                        yield Ok(StreamOutput::TaskStart { id, name });
                    }
                    Ok(ExecutionEvent::TaskEnd { id, name, result })
                        if modes.contains(&StreamMode::Tasks) =>
                    {
                        yield Ok(StreamOutput::TaskEnd { id, name, result });
                    }
                    Ok(ExecutionEvent::TaskFailed { id, name, error })
                        if modes.contains(&StreamMode::Tasks) =>
                    {
                        yield Ok(StreamOutput::TaskFailed { id, name, error });
                    }
                    Ok(_) => {}
                    Err(error) => yield Err(error),
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
    pub async fn invoke_parallel(&self, input: S) -> Result<S, GraphError> {
        self.ainvoke(input).await
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

        outgoing.sort();
        outgoing
    }
}
