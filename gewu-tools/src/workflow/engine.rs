//! Runs one workflow: the spec is compiled into a `gewu-graph`
//! `StateGraph<WorkflowState>` and executed with `ainvoke`.
//!
//! Node handlers drive agents exactly like the task tool does (own
//! session, streamed to a final response), plus the review/redo loop and
//! the run-level limits: task budget, concurrency bound, timeout, and
//! cancel. Progress is mirrored into the shared [`RunState`] as nodes
//! start and complete, so the manager can snapshot mid-run.

use crate::task::{AgentFactory, AgentOverrides};
use crate::workflow::graph_state::{NodeOutcome, TaskFailure, WorkflowState};
use crate::workflow::planning::{PlannedEdge, plan_graph, plan_items};
use crate::workflow::spec::{MAX_TASKS_PER_RUN, NodeSpec, ReviewSpec, WorkflowSpec};
use crate::workflow::template::{TemplateVars, render};
use gewu_core::types::AgentEvent;
use gewu_graph::{CompiledGraph, NodeResult, StateGraph};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// Terminal status of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    #[default]
    Running,
    Completed,
    Canceled,
    TimedOut,
    /// The graph itself failed (e.g. the superstep limit was hit); the
    /// message is in `RunState::error` and completed nodes keep their
    /// results.
    Failed,
}

/// Live state of a run, shared between the engine and the manager.
#[derive(Debug, Clone, Default)]
pub struct RunState {
    pub status: RunStatus,
    /// Node currently executing (or the last one started).
    pub current_node: Option<String>,
    /// Declared node ids in spec order; `nodes` is keyed by id for lookups.
    pub order: Vec<String>,
    /// Completed nodes' outcomes, keyed by node id.
    pub nodes: BTreeMap<String, NodeOutcome>,
    pub tasks_done: usize,
    pub tasks_failed: usize,
    /// Agent calls attempted, counting reviewer calls and redos.
    pub tasks_attempted: usize,
    /// Why the graph failed, when `status` is `Failed`.
    pub error: Option<String>,
}

impl RunState {
    /// The last completed node's outputs, newline-joined.
    pub fn final_output(&self) -> String {
        self.order
            .iter()
            .rev()
            .find_map(|id| self.nodes.get(id))
            .map(|outcome| outcome.outputs.join("\n\n"))
            .unwrap_or_default()
    }
}

/// Resolves an agent name to the (base profile, overrides) pair to spawn:
/// inline spec agents map onto their base with their overrides, predefined
/// profiles pass through unchanged.
fn resolve_agent(name: &str, spec: &WorkflowSpec) -> (String, AgentOverrides) {
    match spec.agents.get(name) {
        Some(inline) => (
            inline.base.clone(),
            AgentOverrides {
                instructions: inline.instructions.clone(),
                tools: inline.tools.clone(),
            },
        ),
        None => (name.to_string(), AgentOverrides::default()),
    }
}

/// Compiles the spec into an executable graph. Node handlers reuse the
/// shared [`RunState`] to mirror progress; the graph state itself carries
/// node outcomes for templates and routing.
fn compile(
    spec: &Arc<WorkflowSpec>,
    factory: Arc<dyn AgentFactory>,
    parent_session: String,
    working_directory: Option<PathBuf>,
    budget: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<RunState>>,
) -> CompiledGraph<WorkflowState> {
    let plan = plan_graph(spec);
    let mut graph: StateGraph<WorkflowState> = StateGraph::new();

    for node in &spec.nodes {
        let node = node.clone();
        let node_id = node.id.clone();
        let spec = spec.clone();
        let factory = factory.clone();
        let parent_session = parent_session.clone();
        let budget = budget.clone();
        let cancel = cancel.clone();
        let state = state.clone();
        let working_directory = working_directory.clone();
        graph.add_node(&node_id, move |graph_state: WorkflowState| {
            let node = node.clone();
            let spec = spec.clone();
            let factory = factory.clone();
            let parent_session = parent_session.clone();
            let budget = budget.clone();
            let cancel = cancel.clone();
            let state = state.clone();
            let working_directory = working_directory.clone();
            async move {
                // A canceled run unwinds through the remaining nodes
                // without recording outcomes (they stay "skipped").
                if cancel.load(Ordering::SeqCst) {
                    return NodeResult::Partial(WorkflowState::default());
                }
                state.lock().expect("run state mutex poisoned").current_node =
                    Some(node.id.clone());
                let outcome = run_node(
                    &node,
                    &spec,
                    graph_state,
                    factory,
                    parent_session,
                    working_directory,
                    budget,
                    cancel,
                    state.clone(),
                )
                .await;
                {
                    let mut guard = state.lock().expect("run state mutex poisoned");
                    guard.tasks_done += outcome.outputs.len();
                    guard.tasks_failed += outcome.failed.len();
                    guard.nodes.insert(node.id.clone(), outcome.clone());
                }
                let mut partial = WorkflowState::default();
                partial.nodes.insert(node.id.clone(), outcome);
                NodeResult::Partial(partial)
            }
        });
    }

    for edge in plan.edges {
        match edge {
            PlannedEdge::Direct { source, target } => {
                graph.add_edge(&source, &target);
            }
            PlannedEdge::Join { sources, target } => {
                graph.add_waiting_edge(sources, &target);
            }
        }
    }

    for route in plan.routes {
        let source = route.source.clone();
        graph.add_conditional_edges(
            &source,
            move |state: &WorkflowState| route.target(state),
            HashMap::new(),
        );
    }

    graph.set_entry_point(&plan.entry);
    graph.set_max_steps(plan.max_steps);
    graph.compile()
}

/// Runs the spec to completion (or cancel/timeout), updating `state` as it
/// goes. The timeout aborts in-flight tasks; cancel is cooperative between
/// nodes and immediate at the task level.
pub async fn run(
    spec: WorkflowSpec,
    factory: Arc<dyn AgentFactory>,
    parent_session: String,
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<RunState>>,
) {
    run_in(
        spec,
        factory,
        parent_session,
        None,
        cancel,
        state,
    )
    .await;
}

pub async fn run_in(
    spec: WorkflowSpec,
    factory: Arc<dyn AgentFactory>,
    parent_session: String,
    working_directory: Option<PathBuf>,
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<RunState>>,
) {
    let timeout = Duration::from_secs(spec.timeout_secs());
    let budget = Arc::new(AtomicUsize::new(0));
    {
        let mut guard = state.lock().expect("run state mutex poisoned");
        guard.order = spec.nodes.iter().map(|node| node.id.clone()).collect();
    }
    if timeout.is_zero() {
        state.lock().expect("run state mutex poisoned").status = RunStatus::TimedOut;
        return;
    }

    let spec = Arc::new(spec);
    let graph = compile(
        &spec,
        factory,
        parent_session,
        working_directory,
        budget.clone(),
        cancel.clone(),
        state.clone(),
    );

    let status = match tokio::time::timeout(timeout, graph.ainvoke(WorkflowState::default())).await
    {
        Err(_) => RunStatus::TimedOut,
        Ok(Err(error)) => {
            state.lock().expect("run state mutex poisoned").error = Some(error.to_string());
            RunStatus::Failed
        }
        Ok(Ok(_)) => RunStatus::Completed,
    };

    let status = if status == RunStatus::Completed && cancel.load(Ordering::SeqCst) {
        RunStatus::Canceled
    } else {
        status
    };

    let mut guard = state.lock().expect("run state mutex poisoned");
    guard.status = status;
    guard.tasks_attempted = budget.load(Ordering::SeqCst);
}

/// Runs one node: resolves its items from the graph state, renders each
/// prompt, and executes the tasks in parallel bounded by the spec's
/// concurrency.
#[allow(clippy::too_many_arguments)]
async fn run_node(
    node: &NodeSpec,
    spec: &Arc<WorkflowSpec>,
    graph_state: WorkflowState,
    factory: Arc<dyn AgentFactory>,
    parent_session: String,
    working_directory: Option<PathBuf>,
    budget: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<RunState>>,
) -> NodeOutcome {
    let items = plan_items(node, &graph_state);

    let semaphore = Arc::new(Semaphore::new(spec.concurrency()));
    let mut join_set: JoinSet<(usize, TaskOutcome)> = JoinSet::new();

    for (index, item) in items.iter().enumerate() {
        let (base, overrides) = resolve_agent(&node.agent, spec);
        match item.prompt.clone() {
            Ok(prompt) => join_set.spawn(run_task(
                factory.clone(),
                parent_session.clone(),
                working_directory.clone(),
                base,
                overrides,
                prompt,
                node.review.clone(),
                spec.clone(),
                budget.clone(),
                cancel.clone(),
                state.clone(),
                semaphore.clone(),
                index,
            )),
            Err(error) => join_set.spawn(async move { (index, Err((error, None))) }),
        };
    }

    let mut results: Vec<Option<TaskOutcome>> = (0..items.len()).map(|_| None).collect();
    while let Some(joined) = join_set.join_next().await {
        if let Ok((index, outcome)) = joined {
            results[index] = Some(outcome);
        }
    }

    let mut outcome = NodeOutcome::default();
    for (index, slot) in results.into_iter().enumerate() {
        let Some(result) = slot else {
            outcome.failed.push(TaskFailure {
                index,
                item: items[index].item.clone().unwrap_or(serde_json::Value::Null),
                error: "the task did not produce a result (execution task lost)".to_string(),
            });
            outcome.sessions.push(String::new());
            continue;
        };
        outcome.sessions.push(session_of(&result));
        match result {
            Ok((output, _)) => outcome.outputs.push(output),
            Err((error, _)) => outcome.failed.push(TaskFailure {
                index,
                item: items[index].item.clone().unwrap_or(serde_json::Value::Null),
                error,
            }),
        }
    }
    outcome
}

/// One task's outcome plus the subagent session id it ran in (the session
/// is known even when the task failed after agent creation).
type TaskOutcome = Result<(String, String), (String, Option<String>)>;

/// Extracts the session id recorded alongside the outcome.
fn session_of(result: &TaskOutcome) -> String {
    match result {
        Ok((_, session)) => session.clone(),
        Err((_, Some(session))) => session.clone(),
        Err((_, None)) => String::new(),
    }
}

/// Executes one task: budget check, agent run, then the optional
/// review/redo loop.
#[allow(clippy::too_many_arguments)]
async fn run_task(
    factory: Arc<dyn AgentFactory>,
    parent_session: String,
    working_directory: Option<PathBuf>,
    base: String,
    overrides: AgentOverrides,
    prompt: String,
    review: Option<ReviewSpec>,
    spec: Arc<WorkflowSpec>,
    budget: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<RunState>>,
    semaphore: Arc<Semaphore>,
    index: usize,
) -> (usize, TaskOutcome) {
    let _permit = semaphore.acquire_owned().await;
    if cancel.load(Ordering::SeqCst) {
        return (index, Err(("canceled".to_string(), None)));
    }

    let result = execute_with_review(
        &factory,
        &parent_session,
        working_directory,
        &base,
        &overrides,
        &prompt,
        review.as_ref(),
        &spec,
        &budget,
        &cancel,
    )
    .await;

    state
        .lock()
        .expect("run state mutex poisoned")
        .tasks_attempted = budget.load(Ordering::SeqCst);

    (index, result)
}

/// Runs the task agent, then the reviewer loop.
#[allow(clippy::too_many_arguments)]
async fn execute_with_review(
    factory: &Arc<dyn AgentFactory>,
    parent_session: &str,
    working_directory: Option<PathBuf>,
    base: &str,
    overrides: &AgentOverrides,
    prompt: &str,
    review: Option<&ReviewSpec>,
    spec: &WorkflowSpec,
    budget: &Arc<AtomicUsize>,
    cancel: &Arc<AtomicBool>,
) -> TaskOutcome {
    let charge = |budget: &AtomicUsize| -> Result<(), (String, Option<String>)> {
        let attempted = budget.fetch_add(1, Ordering::SeqCst) + 1;
        if attempted > MAX_TASKS_PER_RUN {
            return Err((
                format!("workflow task budget exhausted ({MAX_TASKS_PER_RUN} agent calls per run)"),
                None,
            ));
        }
        Ok(())
    };

    let mut prompt = prompt.to_string();
    let mut attempts = 0u32;
    let max_attempts = review.map_or(1, |review| 1 + review.max_redos);
    loop {
        charge(budget)?;
        if cancel.load(Ordering::SeqCst) {
            return Err(("canceled".to_string(), None));
        }
        let (mut agent, session) = factory
            .create_with_in(base, overrides, parent_session, working_directory.clone())
            .await
            .map_err(|error| (format!("failed to create agent '{base}': {error}"), None))?;
        let output = drive_agent(&mut agent, &prompt)
            .await
            .map_err(|error| (error, Some(session.clone())))?;

        let Some(review) = review else {
            return Ok((output, session));
        };

        charge(budget)?;
        let (reviewer_base, reviewer_overrides) = resolve_agent(&review.agent, spec);
        let (mut reviewer, _) = factory
            .create_with_in(
                &reviewer_base,
                &reviewer_overrides,
                parent_session,
                working_directory.clone(),
            )
            .await
            .map_err(|error| {
                (
                    format!("failed to create reviewer '{}': {error}", review.agent),
                    Some(session.clone()),
                )
            })?;
        let review_vars = TemplateVars {
            output: Some(output.clone()),
            ..Default::default()
        };
        let review_prompt = render(&review.prompt, &review_vars).map_err(|error| {
            (
                format!("template error in review prompt: {error}"),
                Some(session.clone()),
            )
        })?;
        let verdict = drive_agent(&mut reviewer, &review_prompt)
            .await
            .map_err(|error| (error, Some(session.clone())))?;

        if verdict.trim_start().starts_with(&review.pass_if) {
            return Ok((output, session));
        }

        attempts += 1;
        if attempts >= max_attempts {
            return Err((
                format!(
                    "review rejected after {} redo(s): {verdict}",
                    review.max_redos
                ),
                Some(session),
            ));
        }
        prompt = format!(
            "{prompt}\n\nA reviewer rejected the previous attempt with this feedback:\n{verdict}\nRedo the task and address the feedback."
        );
    }
}

/// Drives an agent to its final response, collecting the streamed text
/// (same consumption pattern as the task tool).
async fn drive_agent(agent: &mut gewu_core::Agent, prompt: &str) -> Result<String, String> {
    use futures_util::StreamExt;
    let mut stream = Box::pin(agent.ask(prompt));
    let mut result = String::new();
    while let Some(event_result) = stream.next().await {
        match event_result {
            Ok(AgentEvent::ContentDelta { delta }) => result.push_str(&delta),
            Ok(AgentEvent::Finish { response }) => result = response,
            Ok(AgentEvent::Error { error }) => return Err(error),
            Err(error) => return Err(error.to_string()),
            Ok(_) => {}
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::engine;
    use crate::workflow::testutil::{ERROR_MARKER, ScriptedFactory};
    use serde_json::json;
    use std::time::{Duration, Instant};

    /// Runs `spec` to completion on a zero-delay factory and returns the
    /// shared run state.
    async fn run_spec(spec: serde_json::Value, factory: &Arc<ScriptedFactory>) -> RunState {
        let spec: WorkflowSpec = serde_json::from_value(spec).expect("spec should parse");
        let state = Arc::new(Mutex::new(RunState::default()));
        engine::run(
            spec,
            factory.clone(),
            "parent".to_string(),
            Arc::new(AtomicBool::new(false)),
            state.clone(),
        )
        .await;
        let guard = state.lock().unwrap();
        guard.clone()
    }

    #[tokio::test]
    async fn absent_edges_chain_nodes_in_order() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.script("build", &["first finding"]);
        factory.script("general", &["combined"]);

        let state = run_spec(
            json!({
                "nodes": [
                    {"id": "read", "agent": "build", "for_each": ["a", "b"], "prompt": "read {{item}}"},
                    {"id": "combine", "agent": "general",
                     "prompt": "combine {{nodes.read.outputs}}"}
                ]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        assert_eq!(state.tasks_done, 3);
        assert_eq!(
            state.nodes["read"].outputs,
            vec!["first finding", "first finding"]
        );
        assert_eq!(state.nodes["combine"].outputs, vec!["combined"]);
        assert_eq!(state.final_output(), "combined");

        let calls = factory.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls.iter().all(|call| call.parent_session == "parent"));
    }

    #[tokio::test]
    async fn from_node_uses_earlier_outputs_as_items() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.script("build", &["out-1", "out-2", "check-1", "check-2"]);

        let state = run_spec(
            json!({
                "nodes": [
                    {"id": "produce", "agent": "build", "for_each": ["x", "y"], "prompt": "p"},
                    {"id": "verify", "agent": "build", "for_each": {"from": "produce"},
                     "prompt": "verify {{item}} #{{index}}"}
                ]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        assert_eq!(state.nodes["produce"].outputs, vec!["out-1", "out-2"]);
        // One task per output of the producing node, in output order.
        assert_eq!(state.nodes["verify"].outputs, vec!["check-1", "check-2"]);
    }

    #[tokio::test]
    async fn explicit_edges_replace_the_chain() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.script("build", &["a-out", "c-out"]);

        let state = run_spec(
            json!({
                "nodes": [
                    {"id": "a", "agent": "build", "prompt": "p"},
                    {"id": "b", "agent": "build", "prompt": "p"},
                    {"id": "c", "agent": "build", "prompt": "p"}
                ],
                "edges": [["a", "c"]]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        // a -> c directly; b is unreachable and stays skipped.
        assert_eq!(state.nodes["a"].outputs, vec!["a-out"]);
        assert_eq!(state.nodes["c"].outputs, vec!["c-out"]);
        assert!(!state.nodes.contains_key("b"));
        assert_eq!(state.order, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn a_join_waits_for_all_of_its_sources() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.script("build", &["split-out", "merged"]);
        factory.script("explore", &["left-out"]);
        factory.script("general", &["right-out"]);

        let state = run_spec(
            json!({
                "nodes": [
                    {"id": "split", "agent": "build", "prompt": "p"},
                    {"id": "left", "agent": "explore", "prompt": "p"},
                    {"id": "right", "agent": "general", "prompt": "p"},
                    {"id": "merge", "agent": "build",
                     "prompt": "L={{nodes.left.outputs}} R={{nodes.right.outputs}}"}
                ],
                "edges": [["split", "left"], ["split", "right"], ["left", "merge"], ["right", "merge"]]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        // merge's prompt references both branches: rendering succeeded
        // only because both outcomes existed when it ran, and the call
        // count shows it ran exactly once (the join barrier held).
        assert_eq!(state.nodes["merge"].outputs, vec!["merged"]);
        assert_eq!(factory.calls.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn conditional_routing_retries_until_ok() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.script("build", &[&format!("{ERROR_MARKER}boom"), "fine"]);
        factory.script("general", &["done-out"]);

        let state = run_spec(
            json!({
                "nodes": [
                    {"id": "work", "agent": "build", "prompt": "p"},
                    {"id": "done", "agent": "general", "prompt": "p"}
                ],
                "conditional_edges": {"work": {"path": "{{nodes.work.status}}",
                    "map": {"failed": "work", "ok": "done"}}}
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        assert_eq!(state.nodes["work"].outputs, vec!["fine"]);
        // The re-execution overwrote work's outcome; the run counters
        // still account for the failed first pass.
        assert_eq!(state.nodes["done"].outputs, vec!["done-out"]);
        assert_eq!(state.tasks_done, 2);
        assert_eq!(state.tasks_failed, 1);
    }

    #[tokio::test]
    async fn an_unmapped_route_ends_the_run() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.script("build", &[&format!("{ERROR_MARKER}boom"), "fine"]);
        factory.script("general", &["done-out"]);

        let state = run_spec(
            json!({
                "nodes": [
                    {"id": "work", "agent": "build", "prompt": "p"},
                    {"id": "done", "agent": "general", "prompt": "p"}
                ],
                // Only failures loop back; a passing run ends the graph.
                "conditional_edges": {"work": {"path": "{{nodes.work.status}}",
                    "map": {"failed": "work"}}}
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        assert_eq!(state.nodes["work"].outputs, vec!["fine"]);
        assert!(!state.nodes.contains_key("done"));
        assert!(factory.calls.lock().unwrap().len() == 2);
    }

    #[tokio::test]
    async fn the_star_key_catches_every_other_value() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.script("build", &[&format!("{ERROR_MARKER}boom"), "fine"]);
        factory.script("general", &["done-out"]);

        let state = run_spec(
            json!({
                "nodes": [
                    {"id": "work", "agent": "build", "prompt": "p"},
                    {"id": "done", "agent": "general", "prompt": "p"}
                ],
                "conditional_edges": {"work": {"path": "{{nodes.work.failed_count}}",
                    "map": {"0": "done", "*": "work"}}}
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        assert_eq!(state.nodes["work"].outputs, vec!["fine"]);
        assert_eq!(state.nodes["done"].outputs, vec!["done-out"]);
    }

    #[tokio::test]
    async fn routing_replaces_a_routers_plain_edges() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.script("build", &["work-out"]);
        factory.script("general", &["done-out"]);

        let state = run_spec(
            json!({
                "nodes": [
                    {"id": "work", "agent": "build", "prompt": "p"},
                    {"id": "side", "agent": "build", "prompt": "p"},
                    {"id": "done", "agent": "general", "prompt": "p"}
                ],
                // Auto-chain would give work -> side; the router wins.
                "conditional_edges": {"work": {"path": "{{nodes.work.status}}",
                    "map": {"ok": "done"}}}
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        assert!(!state.nodes.contains_key("side"));
        assert_eq!(state.nodes["done"].outputs, vec!["done-out"]);
    }

    #[tokio::test]
    async fn concurrency_bound_holds() {
        let factory = Arc::new(ScriptedFactory::new(Duration::from_millis(100)));
        let state = run_spec(
            json!({
                "max_concurrency": 2,
                "nodes": [
                    {"id": "wide", "agent": "build", "for_each": [1, 2, 3, 4, 5, 6], "prompt": "p"}
                ]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        assert_eq!(factory.peak.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn inline_agent_overrides_reach_the_factory() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));

        let state = run_spec(
            json!({
                "agents": {
                    "specialist": {
                        "base": "build",
                        "instructions": "Be terse.",
                        "tools": ["read_file", "grep"]
                    }
                },
                "nodes": [{"id": "work", "agent": "specialist", "prompt": "p"}]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        let calls = factory.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].base, "build");
        assert_eq!(calls[0].instructions.as_deref(), Some("Be terse."));
        assert_eq!(
            calls[0].tools,
            Some(vec!["read_file".to_string(), "grep".to_string()])
        );
    }

    #[tokio::test]
    async fn create_and_stream_failures_become_task_failures() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.fail_create("build");
        factory.fail_stream("general");

        let state = run_spec(
            json!({
                "nodes": [
                    {"id": "create-fail", "agent": "build", "prompt": "p"},
                    {"id": "stream-fail", "agent": "general", "prompt": "p"}
                ]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        assert_eq!(state.tasks_done, 0);
        assert_eq!(state.tasks_failed, 2);
        let create_error = &state.nodes["create-fail"].failed[0].error;
        assert!(
            create_error.contains("failed to create agent 'build'"),
            "{create_error}"
        );
        assert!(
            !state.nodes["stream-fail"].failed[0].error.is_empty(),
            "the stream failure surfaces as a task failure"
        );
    }

    #[tokio::test]
    async fn one_failing_task_does_not_fail_the_run() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.script("build", &[&format!("{ERROR_MARKER}boom"), "fine"]);

        let state = run_spec(
            json!({
                "nodes": [
                    {"id": "work", "agent": "build", "for_each": ["a", "b"], "prompt": "p"}
                ]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        assert_eq!(state.tasks_done, 1);
        assert_eq!(state.tasks_failed, 1);
        let failed = &state.nodes["work"].failed;
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].index, 0);
        assert_eq!(failed[0].item, json!("a"));
        assert!(failed[0].error.contains("boom"));
        // The failed task's session is still recorded.
        assert_eq!(state.nodes["work"].sessions.len(), 2);
        assert!(!state.nodes["work"].sessions[0].is_empty());
    }

    #[tokio::test]
    async fn review_passes_on_the_first_try() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.script("build", &["the draft"]);
        factory.script("general", &["PASS looks good"]);

        let state = run_spec(
            json!({
                "nodes": [
                    {"id": "write", "agent": "build", "prompt": "write",
                     "review": {"agent": "general", "prompt": "check {{output}}"}}
                ]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        assert_eq!(state.nodes["write"].outputs, vec!["the draft"]);
        assert_eq!(state.tasks_attempted, 2);
        let calls = factory.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].base, "build");
        assert_eq!(calls[1].base, "general");
    }

    #[tokio::test]
    async fn review_redo_retries_with_feedback_then_passes() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.script("build", &["draft one", "draft two"]);
        factory.script("general", &["FAIL needs work", "PASS now good"]);

        let state = run_spec(
            json!({
                "nodes": [
                    {"id": "write", "agent": "build", "prompt": "write",
                     "review": {"agent": "general", "prompt": "check {{output}}",
                                "pass_if": "PASS", "max_redos": 1}}
                ]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        assert_eq!(state.nodes["write"].outputs, vec!["draft two"]);
        // Worker + reviewer + redo worker + redo reviewer.
        assert_eq!(state.tasks_attempted, 4);
        assert_eq!(factory.calls.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn review_redo_exhaustion_fails_the_task() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.script("build", &["draft one", "draft two"]);
        factory.script("general", &["FAIL no"]);

        let state = run_spec(
            json!({
                "nodes": [
                    {"id": "write", "agent": "build", "prompt": "write",
                     "review": {"agent": "general", "prompt": "check {{output}}",
                                "max_redos": 1}}
                ]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        assert_eq!(state.tasks_done, 0);
        assert_eq!(state.tasks_failed, 1);
        let failed = &state.nodes["write"].failed;
        assert!(failed[0].error.contains("review rejected after 1 redo(s)"));
        assert!(failed[0].error.contains("FAIL no"));
    }

    #[tokio::test]
    async fn a_zero_timeout_times_out_before_any_task() {
        let factory = Arc::new(ScriptedFactory::new(Duration::from_millis(50)));
        let state = run_spec(
            json!({
                "timeout_secs": 0,
                "nodes": [{"id": "work", "agent": "build", "prompt": "p"}]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::TimedOut);
        assert_eq!(state.tasks_attempted, 0);
        assert!(factory.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_step_limit_fails_runaway_loops() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.script("build", &["ok"]);

        let state = run_spec(
            json!({
                "max_steps": 3,
                "nodes": [{"id": "work", "agent": "build", "prompt": "p"}],
                "edges": [["work", "work"]]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Failed);
        let error = state.error.expect("the failure explains itself");
        assert!(error.contains("step limit"), "{error}");
        // Three supersteps ran before the limit ended the loop; each
        // execution overwrote the node's outcome, the counter does not.
        assert_eq!(state.nodes["work"].outputs, vec!["ok"]);
        assert_eq!(state.tasks_attempted, 3);
    }

    #[tokio::test]
    async fn cancel_stops_the_run() {
        let factory = Arc::new(ScriptedFactory::new(Duration::from_millis(300)));
        let spec: WorkflowSpec = serde_json::from_value(json!({
            "nodes": [
                {"id": "first", "agent": "build", "for_each": ["a", "b"], "prompt": "p"},
                {"id": "second", "agent": "build", "prompt": "p"}
            ]
        }))
        .unwrap();
        let state = Arc::new(Mutex::new(RunState::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(engine::run(
            spec,
            factory.clone(),
            "parent".to_string(),
            cancel.clone(),
            state.clone(),
        ));

        // Wait until a task is actually executing, then cancel: the first
        // node still finishes (its tasks are in flight), the second never
        // records an outcome.
        while factory.running.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cancel.store(true, Ordering::SeqCst);
        handle.await.unwrap();

        let guard = state.lock().unwrap();
        assert_eq!(guard.status, RunStatus::Canceled);
        assert!(guard.nodes.contains_key("first"));
        assert!(!guard.nodes.contains_key("second"));
        assert_eq!(guard.tasks_attempted, 2);
    }

    #[tokio::test]
    async fn entry_point_selects_the_first_node() {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.script("general", &["second first"]);
        factory.script("build", &["then this"]);

        let state = run_spec(
            json!({
                "entry": "second",
                "nodes": [
                    {"id": "first", "agent": "build", "prompt": "p"},
                    {"id": "second", "agent": "general", "prompt": "p"},
                    {"id": "third", "agent": "build", "prompt": "p"}
                ],
                "edges": [["second", "third"]]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        // second ran first and routed to third; first is unreachable.
        assert_eq!(state.nodes["second"].outputs, vec!["second first"]);
        assert_eq!(state.nodes["third"].outputs, vec!["then this"]);
        assert!(!state.nodes.contains_key("first"));
    }

    #[tokio::test]
    async fn references_to_nodes_that_have_not_run_fail_the_task() {
        // The entry node references itself: on its first pass there is no
        // outcome yet, so the prompt cannot render.
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        factory.script("build", &["unused"]);

        let state = run_spec(
            json!({
                "nodes": [{"id": "work", "agent": "build",
                           "prompt": "prior={{nodes.work.outputs}}"}]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::Completed);
        let error = &state.nodes["work"].failed[0].error;
        assert!(error.contains("template error"), "{error}");
        assert!(error.contains("has not run yet"), "{error}");
        assert!(factory.calls.lock().unwrap().is_empty());
    }

    /// Guards against the compile step regressing into unbounded loops:
    /// the timeout must abort in-flight node work.
    #[tokio::test]
    async fn timeout_aborts_mid_run_nodes() {
        let factory = Arc::new(ScriptedFactory::new(Duration::from_secs(60)));
        let started = Instant::now();
        let state = run_spec(
            json!({
                "timeout_secs": 1,
                "nodes": [{"id": "slow", "agent": "build", "prompt": "p"}]
            }),
            &factory,
        )
        .await;

        assert_eq!(state.status, RunStatus::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
