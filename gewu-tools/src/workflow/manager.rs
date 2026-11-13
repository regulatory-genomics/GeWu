//! Tracks background workflow runs.
//!
//! [`WorkflowManager`] validates specs, spawns runs as detached Tokio
//! tasks, and answers status/wait/cancel queries. Runs are keyed by a
//! generated `run_id` and owned by the session that started them; dropping
//! the manager aborts every run it still holds.

use crate::task::{AgentFactory, AgentOverrides};
use crate::workflow::engine::{self, RunState, RunStatus};
use crate::workflow::graph_state::TaskFailure;
use crate::workflow::spec::WorkflowSpec;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// Concurrent runs allowed per session.
pub const MAX_RUNS_PER_SESSION: usize = 3;
/// `workflow_wait` wait time when the call does not set one.
pub const DEFAULT_WAIT_SECS: u64 = 60;
/// Hard cap on a single `workflow_wait` call.
pub const MAX_WAIT_SECS: u64 = 600;

/// One node's report in a run snapshot. A node the graph never reached is
/// reported with status `pending` (run in progress) or `skipped` (run
/// finished without executing it).
#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeReport {
    pub id: String,
    pub status: String,
    pub done: usize,
    pub failed: Vec<TaskFailure>,
    /// Subagent session id of every attempted task.
    pub sessions: Vec<String>,
}

/// A point-in-time view of a run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunSnapshot {
    pub run_id: String,
    pub name: Option<String>,
    pub status: RunStatus,
    pub current_node: Option<String>,
    pub tasks_done: usize,
    pub tasks_failed: usize,
    pub tasks_attempted: usize,
    pub nodes: Vec<NodeReport>,
    /// The last completed node's outputs; present once any node finished.
    pub final_output: Option<String>,
    /// Why the graph failed, when `status` is `failed`.
    pub error: Option<String>,
}

struct RunHandle {
    owner_session: String,
    name: Option<String>,
    handle: JoinHandle<()>,
    /// Cooperative stop signal; `cancel` also aborts the task directly.
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<RunState>>,
}

/// Owns all workflow runs of the tools it backs.
pub struct WorkflowManager {
    factory: Arc<dyn AgentFactory>,
    /// Predefined profiles workflow agents may use as a base.
    builtin_agents: Vec<String>,
    runs: Mutex<HashMap<String, RunHandle>>,
}

impl WorkflowManager {
    pub fn new(factory: Arc<dyn AgentFactory>, builtin_agents: Vec<String>) -> Self {
        Self {
            factory,
            builtin_agents,
            runs: Mutex::new(HashMap::new()),
        }
    }

    /// Validates the spec and spawns the run, returning the `run_id`
    /// immediately. Validation errors name the problem so the model can
    /// fix the spec.
    pub async fn start(
        &self,
        spec_value: serde_json::Value,
        owner_session: &str,
    ) -> Result<String, String> {
        self.start_with_inbox(spec_value, owner_session, None).await
    }

    pub async fn start_with_inbox(
        &self,
        spec_value: serde_json::Value,
        owner_session: &str,
        inbox: Option<gewu_core::Steering>,
    ) -> Result<String, String> {
        self.start_with_inbox_at(spec_value, owner_session, inbox, None)
            .await
    }

    /// Starts a run and binds its child agents to the selected workspace.
    pub async fn start_with_inbox_at(
        &self,
        spec_value: serde_json::Value,
        owner_session: &str,
        inbox: Option<gewu_core::Steering>,
        working_directory: Option<PathBuf>,
    ) -> Result<String, String> {
        let spec: WorkflowSpec = serde_json::from_value(spec_value)
            .map_err(|error| format!("invalid workflow spec: {error}"))?;
        let builtins: Vec<&str> = self.builtin_agents.iter().map(String::as_str).collect();
        spec.validate(&builtins)
            .map_err(|error| format!("invalid workflow spec: {error}"))?;

        // Every distinct inline agent used by a node or review gets its
        // overrides checked against the factory's real tool sets before
        // anything spawns.
        for (base, overrides) in inline_agents(&spec) {
            self.factory
                .validate_overrides(&base, &overrides)
                .await
                .map_err(|error| format!("invalid workflow spec: {error}"))?;
        }

        let mut runs = self.runs.lock().expect("workflow runs mutex poisoned");
        {
            let active = runs
                .values()
                .filter(|run| {
                    run.owner_session == owner_session
                        && run.state.lock().expect("run state mutex poisoned").status
                            == RunStatus::Running
                })
                .count();
            if active >= MAX_RUNS_PER_SESSION {
                return Err(format!(
                    "too many concurrent workflow runs for this session ({MAX_RUNS_PER_SESSION}); wait for one to finish or cancel one"
                ));
            }
        }

        let run_id = uuid::Uuid::new_v4().to_string();
        let state = Arc::new(Mutex::new(RunState::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let run = engine::run_in(
            spec.clone(),
            self.factory.clone(),
            owner_session.to_string(),
            working_directory,
            cancel.clone(),
            state.clone(),
        );
        let completion_state = state.clone();
        let completion_id = run_id.clone();
        let handle = tokio::spawn(async move {
            use futures_util::FutureExt;
            let result = std::panic::AssertUnwindSafe(run).catch_unwind().await;
            let mut state = completion_state.lock().expect("run state mutex poisoned");
            if result.is_err() {
                state.status = RunStatus::Failed;
                state.error = Some("workflow task panicked".into());
            }
            let status = match state.status {
                RunStatus::Running => "running",
                RunStatus::Completed => "completed",
                RunStatus::Canceled => "canceled",
                RunStatus::TimedOut => "timed out",
                RunStatus::Failed => "failed",
            };
            if let Some(inbox) = inbox {
                inbox.notify(gewu_core::agent::BackgroundNotification {
                    source: "workflow".into(),
                    id: completion_id,
                    status: status.to_owned(),
                    output: format!(
                        "{} done, {} failed\n{}\n{}",
                        state.tasks_done,
                        state.tasks_failed,
                        state.error.as_deref().unwrap_or(""),
                        state.final_output()
                    ),
                });
            }
        });
        runs.insert(
            run_id.clone(),
            RunHandle {
                owner_session: owner_session.to_string(),
                name: spec.name.clone(),
                handle,
                cancel,
                state,
            },
        );
        Ok(run_id)
    }

    /// Returns the run's snapshot without waiting.
    pub fn status(&self, run_id: &str, owner_session: &str) -> Result<RunSnapshot, String> {
        let runs = self.runs.lock().expect("workflow runs mutex poisoned");
        let run = runs.get(run_id).ok_or_else(unknown_run)?;
        if run.owner_session != owner_session {
            return Err(unknown_run());
        }
        Ok(snapshot(run_id, run))
    }

    /// Polls the run until it reaches a terminal status or `wait_secs`
    /// elapse, then returns the snapshot.
    pub async fn wait(
        &self,
        run_id: &str,
        owner_session: &str,
        wait_secs: u64,
    ) -> Result<RunSnapshot, String> {
        let deadline = Instant::now() + Duration::from_secs(wait_secs.min(MAX_WAIT_SECS));
        loop {
            let snapshot = self.status(run_id, owner_session)?;
            if snapshot.status != RunStatus::Running || Instant::now() >= deadline {
                return Ok(snapshot);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Aborts the run and returns whatever partial results exist.
    pub async fn cancel(&self, run_id: &str, owner_session: &str) -> Result<RunSnapshot, String> {
        let state = {
            let runs = self.runs.lock().expect("workflow runs mutex poisoned");
            let run = runs.get(run_id).ok_or_else(unknown_run)?;
            if run.owner_session != owner_session {
                return Err(unknown_run());
            }
            run.cancel.store(true, Ordering::SeqCst);
            run.handle.abort();
            run.state.clone()
        };
        {
            let mut state_guard = state.lock().expect("run state mutex poisoned");
            if state_guard.status == RunStatus::Running {
                state_guard.status = RunStatus::Canceled;
            }
        }
        let runs = self.runs.lock().expect("workflow runs mutex poisoned");
        let run = runs.get(run_id).expect("checked above");
        Ok(snapshot(run_id, run))
    }
}

impl Drop for WorkflowManager {
    fn drop(&mut self) {
        for run in self
            .runs
            .lock()
            .expect("workflow runs mutex poisoned")
            .values()
        {
            run.handle.abort();
        }
    }
}

fn unknown_run() -> String {
    "unknown run_id".to_string()
}

fn snapshot(run_id: &str, run: &RunHandle) -> RunSnapshot {
    let state = run.state.lock().expect("run state mutex poisoned");
    let running = state.status == RunStatus::Running;
    RunSnapshot {
        run_id: run_id.to_string(),
        name: run.name.clone(),
        status: state.status,
        current_node: state.current_node.clone(),
        tasks_done: state.tasks_done,
        tasks_failed: state.tasks_failed,
        tasks_attempted: state.tasks_attempted,
        nodes: state
            .order
            .iter()
            .map(|id| match state.nodes.get(id) {
                Some(outcome) => NodeReport {
                    id: id.clone(),
                    status: outcome.status().to_string(),
                    done: outcome.outputs.len(),
                    failed: outcome.failed.clone(),
                    sessions: outcome.sessions.clone(),
                },
                None => NodeReport {
                    id: id.clone(),
                    status: if running { "pending" } else { "skipped" }.to_string(),
                    done: 0,
                    failed: Vec::new(),
                    sessions: Vec::new(),
                },
            })
            .collect(),
        final_output: (!state.nodes.is_empty()).then(|| state.final_output()),
        error: state.error.clone(),
    }
}

/// Every distinct inline agent (base + overrides) referenced by the spec's
/// nodes and reviews, in deterministic order.
fn inline_agents(spec: &WorkflowSpec) -> Vec<(String, AgentOverrides)> {
    let mut names: Vec<String> = Vec::new();
    for node in &spec.nodes {
        names.push(node.agent.clone());
        if let Some(review) = &node.review {
            names.push(review.agent.clone());
        }
    }
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter_map(|name| {
            spec.agents.get(&name).map(|inline| {
                (
                    inline.base.clone(),
                    AgentOverrides {
                        instructions: inline.instructions.clone(),
                        tools: inline.tools.clone(),
                    },
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::graph_state::NodeOutcome;
    use crate::workflow::testutil::{OWNER, ScriptedFactory};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn setup(delay: Duration) -> (Arc<WorkflowManager>, Arc<ScriptedFactory>) {
        let factory = Arc::new(ScriptedFactory::new(delay));
        let manager = Arc::new(WorkflowManager::new(
            factory.clone(),
            vec!["build".to_string(), "general".to_string()],
        ));
        (manager, factory)
    }

    #[tokio::test]
    async fn completion_notifies_only_the_supplied_session_inbox() {
        let (manager, _) = setup(Duration::ZERO);
        let inbox = gewu_core::Steering::new();
        let other = gewu_core::Steering::new();
        let run_id = manager
            .start_with_inbox_at(spec_value(), OWNER, Some(inbox.clone()), None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), inbox.notified())
            .await
            .unwrap();
        let notifications = inbox.drain();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].contains(&run_id));
        assert!(notifications[0].contains("completed"));
        assert!(!other.is_pending());
        assert_eq!(
            manager.status(&run_id, OWNER).unwrap().status,
            RunStatus::Completed
        );
    }

    fn spec_value() -> serde_json::Value {
        json!({
            "name": "demo",
            "nodes": [{"id": "one", "agent": "build", "prompt": "p"}]
        })
    }

    #[tokio::test]
    async fn start_rejects_invalid_specs_before_spawning() {
        let (manager, factory) = setup(Duration::ZERO);

        let error = manager
            .start(json!({ "nodes": [] }), OWNER)
            .await
            .unwrap_err();
        assert!(error.contains("at least one node"), "{error}");

        let error = manager
            .start(
                json!({"nodes": [{"id": "a", "agent": "ghost", "prompt": "p"}]}),
                OWNER,
            )
            .await
            .unwrap_err();
        assert!(error.contains("unknown agent 'ghost'"), "{error}");
        assert!(factory.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn status_wait_and_cancel_refuse_other_sessions() {
        let (manager, _factory) = setup(Duration::ZERO);
        let run_id = manager.start(spec_value(), OWNER).await.unwrap();

        assert_eq!(
            manager.status(&run_id, "someone-else").unwrap_err(),
            unknown_run()
        );
        assert_eq!(
            manager.wait(&run_id, "someone-else", 1).await.unwrap_err(),
            unknown_run()
        );
        assert_eq!(
            manager.cancel(&run_id, "someone-else").await.unwrap_err(),
            unknown_run()
        );
        // The owner sees the run.
        assert_eq!(manager.status(&run_id, OWNER).unwrap().run_id, run_id);
    }

    #[tokio::test]
    async fn wait_polls_until_terminal_and_reports_counts() {
        let (manager, factory) = setup(Duration::from_millis(150));
        let run_id = manager.start(spec_value(), OWNER).await.unwrap();

        // A zero-second wait on a running run returns immediately.
        let snapshot = manager.wait(&run_id, OWNER, 0).await.unwrap();
        assert_eq!(snapshot.status, RunStatus::Running);

        let snapshot = manager.wait(&run_id, OWNER, 30).await.unwrap();
        assert_eq!(snapshot.status, RunStatus::Completed);
        assert_eq!(snapshot.tasks_done, 1);
        assert_eq!(snapshot.tasks_failed, 0);
        assert_eq!(snapshot.name.as_deref(), Some("demo"));
        assert_eq!(snapshot.nodes[0].id, "one");
        assert_eq!(snapshot.nodes[0].status, "ok");
        assert_eq!(snapshot.nodes[0].sessions, vec!["s-0"]);
        assert_eq!(snapshot.final_output.as_deref(), Some("ok"));
        assert_eq!(factory.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unreachable_nodes_report_as_skipped() {
        let (manager, _factory) = setup(Duration::ZERO);
        let run_id = manager
            .start(
                json!({
                    "nodes": [
                        {"id": "one", "agent": "build", "prompt": "p"},
                        {"id": "never", "agent": "build", "prompt": "p"}
                    ],
                    "edges": [["one", "END"]]
                }),
                OWNER,
            )
            .await
            .unwrap();
        let snapshot = manager.wait(&run_id, OWNER, 30).await.unwrap();

        assert_eq!(snapshot.status, RunStatus::Completed);
        assert_eq!(snapshot.nodes[0].status, "ok");
        assert_eq!(snapshot.nodes[1].id, "never");
        assert_eq!(snapshot.nodes[1].status, "skipped");
        assert_eq!(snapshot.nodes[1].done, 0);
    }

    #[tokio::test]
    async fn cancel_aborts_a_running_run_and_keeps_its_state_readable() {
        let (manager, factory) = setup(Duration::from_secs(60));
        let run_id = manager
            .start(
                json!({
                    "nodes": [
                        {"id": "slow", "agent": "build", "for_each": ["a", "b"], "prompt": "p"}
                    ]
                }),
                OWNER,
            )
            .await
            .unwrap();

        while factory.running.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let snapshot = manager.cancel(&run_id, OWNER).await.unwrap();
        assert_eq!(snapshot.status, RunStatus::Canceled);
        // The run stays queryable after cancellation.
        assert_eq!(
            manager.status(&run_id, OWNER).unwrap().status,
            RunStatus::Canceled
        );
    }

    #[tokio::test]
    async fn sessions_are_capped_at_three_concurrent_runs() {
        let (manager, _factory) = setup(Duration::from_secs(60));
        for _ in 0..MAX_RUNS_PER_SESSION {
            manager
                .start(spec_value(), OWNER)
                .await
                .expect("the first three runs fit under the cap");
        }
        let error = manager.start(spec_value(), OWNER).await.unwrap_err();
        assert!(
            error.contains("too many concurrent workflow runs"),
            "{error}"
        );

        // Another session is not affected by this session's cap.
        manager
            .start(spec_value(), "other-session")
            .await
            .expect("the cap is per session");

        // Cancelling one run frees its slot.
        let first = manager
            .runs
            .lock()
            .unwrap()
            .iter()
            .find(|(_, run)| run.owner_session == OWNER)
            .map(|(id, _)| id.clone())
            .unwrap();
        manager.cancel(&first, OWNER).await.unwrap();
        manager
            .start(spec_value(), OWNER)
            .await
            .expect("a cancelled run frees a slot");
    }

    #[tokio::test]
    async fn inline_overrides_are_validated_against_the_factory() {
        struct RejectingFactory;
        #[async_trait::async_trait]
        impl AgentFactory for RejectingFactory {
            async fn create(
                &self,
                _subagent_type: &str,
                _resume: Option<String>,
                _parent_session: &str,
            ) -> Result<(gewu_core::Agent, String), Box<dyn std::error::Error + Send + Sync>>
            {
                unreachable!("validation should reject before spawning")
            }

            async fn validate_overrides(
                &self,
                base: &str,
                overrides: &crate::task::AgentOverrides,
            ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                if base == "ghost" || overrides.tools.as_deref() == Some(&["bogus".to_string()]) {
                    return Err("rejected".into());
                }
                Ok(())
            }
        }

        let manager = WorkflowManager::new(
            Arc::new(RejectingFactory),
            vec!["build".to_string(), "general".to_string()],
        );
        let error = manager
            .start(
                json!({
                    "agents": {"r": {"base": "ghost"}},
                    "nodes": [{"id": "a", "agent": "r", "prompt": "p"}]
                }),
                OWNER,
            )
            .await
            .unwrap_err();
        assert!(error.contains("rejected"), "{error}");

        let error = manager
            .start(
                json!({
                    "agents": {"r": {"base": "build", "tools": ["bogus"]}},
                    "nodes": [{"id": "a", "agent": "r", "prompt": "p"}]
                }),
                OWNER,
            )
            .await
            .unwrap_err();
        assert!(error.contains("rejected"), "{error}");
    }

    #[tokio::test]
    async fn snapshots_summarize_nodes() {
        let state = RunState {
            status: RunStatus::Completed,
            tasks_done: 2,
            tasks_attempted: 3,
            order: vec!["one".to_string(), "never".to_string()],
            nodes: BTreeMap::from([(
                "one".to_string(),
                NodeOutcome {
                    outputs: vec!["a".to_string(), "b".to_string()],
                    failed: vec![TaskFailure {
                        index: 2,
                        item: json!("c"),
                        error: "boom".to_string(),
                    }],
                    sessions: vec!["s-0".to_string(), "s-1".to_string(), "s-2".to_string()],
                },
            )]),
            ..Default::default()
        };
        let run = RunHandle {
            owner_session: OWNER.to_string(),
            name: Some("demo".to_string()),
            handle: tokio::spawn(async {}),
            cancel: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(state)),
        };
        let snapshot = snapshot("run-1", &run);
        assert_eq!(snapshot.run_id, "run-1");
        assert_eq!(snapshot.tasks_done, 2);
        assert_eq!(snapshot.nodes[0].done, 2);
        assert_eq!(snapshot.nodes[0].status, "failed");
        assert_eq!(snapshot.nodes[0].failed.len(), 1);
        assert_eq!(snapshot.nodes[0].sessions, vec!["s-0", "s-1", "s-2"]);
        assert_eq!(snapshot.nodes[1].status, "skipped");
        assert_eq!(snapshot.final_output.as_deref(), Some("a\n\nb"));
    }
}
