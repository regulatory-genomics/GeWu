//! The state that flows between workflow nodes.
//!
//! [`WorkflowState`] implements [`gewu_graph::State`] for the compiled
//! graph: it carries each executed node's outcome so later nodes (and
//! routing templates) can read them. `merge` upserts per-node outcomes —
//! parallel branches never clobber each other, and a node re-executed by a
//! loop overwrites its previous run.

use gewu_graph::State;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A task that produced no accepted output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskFailure {
    /// Zero-based item index within the node.
    pub index: usize,
    /// The item the task ran for (null for single-task nodes).
    pub item: serde_json::Value,
    /// What went wrong (agent error, review rejection, budget, cancel).
    pub error: String,
}

/// One node execution's outcome.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeOutcome {
    /// Successful task outputs, in item order.
    pub outputs: Vec<String>,
    /// Failed tasks, in item order.
    pub failed: Vec<TaskFailure>,
    /// Subagent session id of every attempted task, indexed by item index
    /// (read transcripts with `gewu-server view <id>`).
    pub sessions: Vec<String>,
}

impl NodeOutcome {
    /// Routing label exposed as `{{nodes.<id>.status}}`: `"ok"` when
    /// nothing failed, `"failed"` otherwise.
    pub fn status(&self) -> &'static str {
        if self.failed.is_empty() {
            "ok"
        } else {
            "failed"
        }
    }

    /// Exposed as `{{nodes.<id>.failed_count}}`.
    pub fn failed_count(&self) -> usize {
        self.failed.len()
    }

    /// The template-addressable view of this outcome.
    pub fn template_data(&self) -> crate::workflow::template::NodeTemplateData {
        crate::workflow::template::NodeTemplateData {
            outputs: self.outputs.clone(),
            failed: self
                .failed
                .iter()
                .map(|failure| serde_json::to_value(failure).unwrap_or_default())
                .collect(),
            status: self.status().to_string(),
        }
    }
}

/// Graph state: outcomes keyed by node id.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowState {
    pub nodes: BTreeMap<String, NodeOutcome>,
}

impl State for WorkflowState {
    type Partial = WorkflowState;

    /// Upserts node outcomes: nodes present in `update` replace their
    /// earlier results, everything else is kept.
    fn merge(&self, update: Self::Partial) -> Self {
        let mut nodes = self.nodes.clone();
        for (id, outcome) in update.nodes {
            nodes.insert(id, outcome);
        }
        WorkflowState { nodes }
    }
}
