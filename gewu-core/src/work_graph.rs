//! Shared Work Graph view model.
//!
//! The Work Graph is a projection layer: it unifies sessions, tasks,
//! subagents, tools, workflows, SOPs, trace runs, timeline events, and
//! artifacts without requiring a dedicated graph database.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct WorkGraph {
    pub root_node_id: String,
    #[serde(default)]
    pub nodes: Vec<WorkNode>,
    #[serde(default)]
    pub edges: Vec<WorkEdge>,
    #[serde(default)]
    pub events: Vec<WorkEvent>,
    #[serde(default)]
    pub artifacts: Vec<WorkArtifact>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorkNode {
    pub id: String,
    pub kind: WorkNodeKind,
    pub title: String,
    pub status: WorkStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
}

impl WorkNode {
    pub fn new(
        id: impl Into<String>,
        kind: WorkNodeKind,
        title: impl Into<String>,
        status: WorkStatus,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            title: title.into(),
            status,
            summary: None,
            started_at: None,
            finished_at: None,
            session_id: None,
            task_id: None,
            trace_run_id: None,
            input: None,
            output: None,
            error: None,
            metadata: Value::Null,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorkEdge {
    pub from: String,
    pub to: String,
    pub kind: WorkEdgeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl WorkEdge {
    pub fn new(from: impl Into<String>, to: impl Into<String>, kind: WorkEdgeKind) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            kind,
            label: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorkEvent {
    pub id: String,
    pub node_id: String,
    pub kind: WorkEventKind,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorkArtifact {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkNodeKind {
    Session,
    AssistantTurn,
    Todo,
    Task,
    Subagent,
    Tool,
    Workflow,
    WorkflowNode,
    Sop,
    Artifact,
    Decision,
    TraceRun,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Canceled,
    Skipped,
    Blocked,
    NeedsReview,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkEdgeKind {
    Contains,
    Calls,
    Spawns,
    WaitsFor,
    Produces,
    Implements,
    DerivedFrom,
    Blocks,
    Recovers,
    TraceOf,
    SessionOf,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkEventKind {
    Created,
    Started,
    Progress,
    Finished,
    Failed,
    Canceled,
    ArtifactProduced,
    DecisionRecorded,
    TraceEvent,
}
