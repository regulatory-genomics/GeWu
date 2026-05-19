use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceConfig {
    pub id: String,
    pub name: String,
    pub project_root: PathBuf,
    pub storage_dir: PathBuf,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionGraph {
    pub session_id: String,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

impl SessionGraph {
    pub fn empty(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GraphNode {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: String,
    pub position: GraphPosition,
    pub data: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GraphEdge {
    pub id: String,
    pub source: String,
    pub target: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub edge_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct GraphPosition {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowEventKind {
    DataIngested,
    StepExecuted,
    ArtifactProduced,
    ArtifactSurfaced,
    ArtifactHidden,
    EdgeDeclared,
    AnnotationAdded,
    UserInteraction,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorkflowEvent {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub actor: String,
    pub kind: WorkflowEventKind,
    pub payload: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArtifactPreview {
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
        total_rows: usize,
    },
    Plot { spec: Value },
    Text { content: String },
    Html {
        uri: String,
        #[serde(default)]
        sandbox: Option<String>,
    },
    Image { uri: String, width: u32, height: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn graph_node_serializes_with_react_flow_type_field() {
        let node = GraphNode {
            id: "n1".to_string(),
            node_type: "data_table".to_string(),
            position: GraphPosition { x: 1.0, y: 2.0 },
            data: json!({"label":"QC"}),
        };
        let value = serde_json::to_value(node).unwrap();
        assert_eq!(value["type"], "data_table");
        assert!(value.get("node_type").is_none());
    }

    #[test]
    fn workflow_event_kind_uses_snake_case_contract() {
        let kind = serde_json::to_value(WorkflowEventKind::ArtifactProduced).unwrap();
        assert_eq!(kind, json!("artifact_produced"));
    }

    #[test]
    fn html_artifact_preview_serializes_with_uri_and_sandbox() {
        let preview = ArtifactPreview::Html {
            uri: "/api/workspaces/default/artifacts/html1/raw".to_string(),
            sandbox: Some("allow-scripts".to_string()),
        };
        let value = serde_json::to_value(preview).unwrap();
        assert_eq!(value["kind"], "html");
        assert_eq!(value["uri"], "/api/workspaces/default/artifacts/html1/raw");
        assert_eq!(value["sandbox"], "allow-scripts");
    }
}
