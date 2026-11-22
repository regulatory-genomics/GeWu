//! The three workflow tools.
//!
//! All three share one [`WorkflowManager`]: `workflow_start` validates the
//! spec and returns a `run_id` immediately, `workflow_status` snapshots a
//! run without waiting, and `workflow_cancel` aborts a run and reports its
//! partial results. Runs publish live progress to the server, which renders
//! it in the GUI.

use crate::workflow::manager::{RunSnapshot, WorkflowManager};
use crate::workflow::spec::WorkflowSpec;
use anyhow::anyhow;
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::Deserialize;
use std::error::Error;
use std::fmt::Display;
use std::sync::Arc;

fn tool_error(message: impl Display) -> Box<dyn Error + Send + Sync> {
    anyhow!("{message}").into()
}

fn normalize_start_args(mut args: serde_json::Value) -> serde_json::Value {
    if let Some(serde_json::Value::String(encoded)) = args.get("spec").cloned()
        && let Ok(inner) = serde_json::from_str::<serde_json::Value>(&encoded)
    {
        args["spec"] = inner;
    }
    args
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkflowStartArgs {
    /// The workflow specification, written as JSON. The schema below
    /// documents every field; invalid specs are rejected before anything
    /// spawns, so a mistake only costs the tool call.
    pub spec: WorkflowSpec,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkflowStatusArgs {
    /// The run_id returned by workflow_start.
    pub run_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkflowCancelArgs {
    /// The run_id returned by workflow_start.
    pub run_id: String,
}

/// Starts a workflow run from a JSON spec; returns the `run_id`
/// immediately while the run executes in the background.
#[derive(Clone)]
pub struct WorkflowStartTool {
    manager: Arc<WorkflowManager>,
}

impl WorkflowStartTool {
    pub fn new(manager: Arc<WorkflowManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for WorkflowStartTool {
    fn name(&self) -> &str {
        "workflow_start"
    }

    fn description(&self) -> &str {
        include_str!("../prompts/workflow.txt")
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(WorkflowStartArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        // Models sometimes wrap the spec as a JSON string; unwrap it once so
        // the call succeeds instead of costing an error-and-retry round trip.
        let input: WorkflowStartArgs = serde_json::from_value(normalize_start_args(args))
            .map_err(|e| tool_error(format!("invalid workflow spec: {e}")))?;
        let spec_value = serde_json::to_value(&input.spec)
            .map_err(|e| tool_error(format!("invalid workflow spec: {e}")))?;
        let run_id = self
            .manager
            .start_with_inbox_at(
                spec_value,
                &ctx.session_id,
                ctx.inbox.clone(),
                ctx.working_directory.clone(),
            )
            .await
            .map_err(tool_error)?;
        let mut result = ToolResult::new(
            "workflow_start",
            format!(
                "Started workflow run {run_id}. It executes in the background; check on it with \
                 workflow_status (run_id: \"{run_id}\"). This session is notified when it \
                 finishes."
            ),
        );
        result.metadata.insert("run_id".into(), run_id.into());
        Ok(result)
    }
}

/// Returns a run's snapshot without waiting.
#[derive(Clone)]
pub struct WorkflowStatusTool {
    manager: Arc<WorkflowManager>,
}

impl WorkflowStatusTool {
    pub fn new(manager: Arc<WorkflowManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for WorkflowStatusTool {
    fn name(&self) -> &str {
        "workflow_status"
    }

    fn description(&self) -> &str {
        "Returns a snapshot of a workflow run without waiting: current node, done/failed \
         counts, each agent's session id, failed tasks, and the output of the last completed \
         node. Use this to check on a run started with workflow_start."
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(WorkflowStatusArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: WorkflowStatusArgs = serde_json::from_value(args).map_err(tool_error)?;
        let snapshot = self
            .manager
            .status(&input.run_id, &ctx.session_id)
            .map_err(tool_error)?;
        Ok(ToolResult::new("workflow_status", snapshot_text(&snapshot)))
    }
}

/// Aborts a run and returns whatever partial results exist.
#[derive(Clone)]
pub struct WorkflowCancelTool {
    manager: Arc<WorkflowManager>,
}

impl WorkflowCancelTool {
    pub fn new(manager: Arc<WorkflowManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for WorkflowCancelTool {
    fn name(&self) -> &str {
        "workflow_cancel"
    }

    fn description(&self) -> &str {
        "Aborts a workflow run and returns whatever partial results exist. Use this when a \
         run went the wrong way or is no longer needed."
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(WorkflowCancelArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: WorkflowCancelArgs = serde_json::from_value(args).map_err(tool_error)?;
        let snapshot = self
            .manager
            .cancel(&input.run_id, &ctx.session_id)
            .await
            .map_err(tool_error)?;
        Ok(ToolResult::new("workflow_cancel", snapshot_text(&snapshot)))
    }
}

/// Renders a snapshot as compact text for the model: one summary line,
/// per-node progress with failures and agent session ids, then the latest
/// completed node's output.
fn snapshot_text(snapshot: &RunSnapshot) -> String {
    let mut text = format!(
        "run {} ({}): {}, {} task(s) done, {} failed, {} attempted",
        snapshot.run_id,
        snapshot.name.as_deref().unwrap_or("unnamed"),
        snapshot.status.label(),
        snapshot.tasks_done,
        snapshot.tasks_failed,
        snapshot.tasks_attempted,
    );
    if let Some(node) = &snapshot.current_node {
        text.push_str(&format!("\ncurrent node: {node}"));
    }
    if let Some(error) = &snapshot.error {
        text.push_str(&format!("\nerror: {error}"));
    }
    for node in &snapshot.nodes {
        text.push_str(&format!(
            "\nnode {}: {}, {} done, {} failed",
            node.id,
            node.status,
            node.done,
            node.failed.len()
        ));
        for failure in &node.failed {
            text.push_str(&format!(
                "\n  task {} failed (item: {}): {}",
                failure.index, failure.item, failure.error
            ));
        }
        if !node.sessions.is_empty() {
            text.push_str(&format!("\n  agent sessions: {}", node.sessions.join(", ")));
        }
    }
    if let Some(output) = &snapshot.final_output
        && !output.is_empty()
    {
        text.push_str(&format!("\nlatest node output:\n{output}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_args_normalization_unwraps_only_once() {
        use serde_json::json;
        let spec = json!({"nodes": []});
        assert_eq!(
            normalize_start_args(json!({"spec": spec.to_string(), "extra": 1})),
            json!({"spec": spec, "extra": 1})
        );
        for args in [
            json!({"spec": spec}),
            json!({"spec": "invalid json"}),
            json!({"other": 1}),
            json!(null),
            json!([]),
        ] {
            assert_eq!(normalize_start_args(args.clone()), args);
        }
        assert_eq!(
            normalize_start_args(json!({"spec": "null"})),
            json!({"spec": null})
        );
        assert_eq!(
            normalize_start_args(json!({"spec": "\"{}\""})),
            json!({"spec": "{}"})
        );
    }
    use crate::workflow::testutil::{OWNER, ScriptedFactory};
    use std::time::Duration;

    fn manager() -> (Arc<WorkflowManager>, Arc<ScriptedFactory>) {
        let factory = Arc::new(ScriptedFactory::new(Duration::ZERO));
        let manager = Arc::new(WorkflowManager::new(
            factory.clone(),
            vec!["build".to_string(), "general".to_string()],
        ));
        (manager, factory)
    }

    fn ctx() -> ToolContext {
        ToolContext::new(OWNER, "msg")
    }

    fn start_args(spec: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "spec": spec })
    }

    /// Extracts the run_id from a workflow_start output.
    fn run_id_from(output: &str) -> String {
        output
            .split_whitespace()
            .nth(3)
            .expect("output names the run")
            .trim_end_matches('.')
            .to_string()
    }

    #[tokio::test]
    async fn start_returns_run_id_and_status_reports_completion() {
        let (manager, _factory) = manager();
        let tool = WorkflowStartTool::new(manager.clone());

        let result = tool
            .execute(
                start_args(serde_json::json!({
                    "name": "demo",
                    "nodes": [{"id": "one", "agent": "build", "prompt": "do it"}]
                })),
                &ctx(),
            )
            .await
            .expect("start should succeed");
        assert!(result.output.contains("Started workflow run "));

        let run_id = run_id_from(&result.output);

        let status = WorkflowStatusTool::new(manager.clone());
        let mut snapshot = status
            .execute(serde_json::json!({ "run_id": run_id }), &ctx())
            .await
            .expect("status should succeed");
        for _ in 0..100 {
            if snapshot.output.contains("completed") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            snapshot = status
                .execute(serde_json::json!({ "run_id": run_id }), &ctx())
                .await
                .expect("status should succeed");
        }
        assert!(snapshot.output.contains("completed"), "{}", snapshot.output);
        assert!(snapshot.output.contains("node one: ok, 1 done"));
    }

    #[tokio::test]
    async fn start_accepts_a_string_encoded_spec() {
        let (manager, _factory) = manager();
        let tool = WorkflowStartTool::new(manager);

        let spec = serde_json::json!({
            "name": "demo",
            "nodes": [{"id": "one", "agent": "build", "prompt": "do it"}]
        });
        let result = tool
            .execute(
                serde_json::json!({ "spec": serde_json::to_string(&spec).unwrap() }),
                &ctx(),
            )
            .await
            .expect("a double-encoded spec should be unwrapped");
        assert!(result.output.contains("Started workflow run "));
    }

    #[tokio::test]
    async fn start_rejects_a_non_json_spec_string() {
        let (manager, _factory) = manager();
        let tool = WorkflowStartTool::new(manager);

        let error = tool
            .execute(serde_json::json!({ "spec": "{\"nodes\": [" }), &ctx())
            .await
            .expect_err("a non-JSON spec string should still fail");
        assert!(
            error.to_string().contains("invalid workflow spec"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn start_rejects_invalid_specs_with_a_useful_error() {
        let (manager, _factory) = manager();
        let tool = WorkflowStartTool::new(manager.clone());

        let error = tool
            .execute(
                start_args(serde_json::json!({
                    "nodes": [{"id": "a", "agent": "nope", "prompt": "p"}]
                })),
                &ctx(),
            )
            .await
            .expect_err("unknown agent should be rejected");
        assert!(
            error.to_string().contains("unknown agent 'nope'"),
            "{error}"
        );

        let error = tool
            .execute(
                start_args(serde_json::json!({
                    "nodes": [{"id": "a", "agent": "build", "prompt": "p"}],
                    "bogus": 1
                })),
                &ctx(),
            )
            .await
            .expect_err("unknown fields should be rejected");
        assert!(
            error.to_string().contains("invalid workflow spec"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn status_and_cancel_report_unknown_runs() {
        let (manager, _factory) = manager();
        let status = WorkflowStatusTool::new(manager.clone());
        let error = status
            .execute(serde_json::json!({ "run_id": "nope" }), &ctx())
            .await
            .expect_err("unknown run should error");
        assert!(error.to_string().contains("unknown run_id"), "{error}");

        let cancel = WorkflowCancelTool::new(manager.clone());
        let error = cancel
            .execute(serde_json::json!({ "run_id": "nope" }), &ctx())
            .await
            .expect_err("unknown run should error");
        assert!(error.to_string().contains("unknown run_id"), "{error}");
    }

    #[tokio::test]
    async fn cancel_returns_partial_results_and_stops_the_run() {
        let (manager, factory) = manager();
        factory.set_delay(Duration::from_secs(60));
        let start = WorkflowStartTool::new(manager.clone());
        let result = start
            .execute(
                start_args(serde_json::json!({
                    "nodes": [
                        {"id": "slow", "agent": "build", "for_each": ["a", "b"], "prompt": "p"}
                    ]
                })),
                &ctx(),
            )
            .await
            .expect("start should succeed");
        let run_id = run_id_from(&result.output);

        let status = WorkflowStatusTool::new(manager.clone());
        let snapshot = status
            .execute(serde_json::json!({ "run_id": run_id }), &ctx())
            .await
            .expect("status should succeed");
        assert!(snapshot.output.contains("running"), "{}", snapshot.output);

        let cancel = WorkflowCancelTool::new(manager.clone());
        let snapshot = cancel
            .execute(serde_json::json!({ "run_id": run_id }), &ctx())
            .await
            .expect("cancel should succeed");
        assert!(snapshot.output.contains("canceled"), "{}", snapshot.output);
    }
}
