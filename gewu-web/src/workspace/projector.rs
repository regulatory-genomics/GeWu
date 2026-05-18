use crate::workspace::index::WorkflowIndex;
use crate::workspace::types::{
    ArtifactPreview, GraphEdge, GraphNode, GraphPosition, WorkflowEvent, WorkflowEventKind,
};
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub enum GraphMutation {
    AddNode(GraphNode),
    AddEdge(GraphEdge),
    RemoveNode(String),
}

#[derive(Default)]
pub struct CanvasProjector {
    surface_map: HashMap<String, String>,
}

impl CanvasProjector {
    pub fn rebuild_from_events(events: &[WorkflowEvent]) -> Self {
        let mut projector = Self::default();
        for event in events {
            match event.kind {
                WorkflowEventKind::ArtifactSurfaced => {
                    if let Some(artifact_id) = event.payload.get("artifact_id").and_then(Value::as_str) {
                        let node_id = event
                            .payload
                            .get("canvas_node_id")
                            .and_then(Value::as_str)
                            .unwrap_or(artifact_id);
                        projector.surface_map.insert(artifact_id.to_string(), node_id.to_string());
                    }
                }
                WorkflowEventKind::ArtifactHidden => {
                    if let Some(artifact_id) = event.payload.get("artifact_id").and_then(Value::as_str) {
                        projector.surface_map.remove(artifact_id);
                    }
                }
                _ => {}
            }
        }
        projector
    }

    pub fn project(&mut self, event: &WorkflowEvent, index: &WorkflowIndex) -> Vec<GraphMutation> {
        match event.kind {
            WorkflowEventKind::ArtifactSurfaced => self.project_artifact_surfaced(event, index),
            WorkflowEventKind::EdgeDeclared => self.project_edge_declared(event),
            WorkflowEventKind::ArtifactHidden => self.project_artifact_hidden(event),
            _ => Vec::new(),
        }
    }

    fn project_artifact_surfaced(
        &mut self,
        event: &WorkflowEvent,
        index: &WorkflowIndex,
    ) -> Vec<GraphMutation> {
        let artifact_id = match event.payload.get("artifact_id").and_then(Value::as_str) {
            Some(id) => id,
            None => return Vec::new(),
        };
        let node_id = event
            .payload
            .get("canvas_node_id")
            .and_then(Value::as_str)
            .unwrap_or(artifact_id);
        let position = event
            .payload
            .get("position")
            .cloned()
            .unwrap_or_else(|| json!({"x": 0.0, "y": 0.0}));
        let x = position.get("x").and_then(Value::as_f64).unwrap_or(0.0);
        let y = position.get("y").and_then(Value::as_f64).unwrap_or(0.0);
        let workflow_node = index.nodes.get(artifact_id);
        let node_type = workflow_node
            .and_then(|node| node.canvas_kind.clone())
            .unwrap_or_else(|| infer_node_type(workflow_node.and_then(|node| node.format.as_deref())));
        let data = build_node_data(artifact_id, workflow_node);
        let node = GraphNode {
            id: node_id.to_string(),
            node_type,
            position: GraphPosition { x, y },
            data,
        };
        self.surface_map
            .insert(artifact_id.to_string(), node_id.to_string());
        vec![GraphMutation::AddNode(node)]
    }

    fn project_edge_declared(&self, event: &WorkflowEvent) -> Vec<GraphMutation> {
        let from = event.payload.get("from_id").and_then(Value::as_str);
        let to = event.payload.get("to_id").and_then(Value::as_str);
        let (Some(from), Some(to)) = (from, to) else {
            return Vec::new();
        };
        let Some(source) = self.surface_map.get(from) else {
            return Vec::new();
        };
        let Some(target) = self.surface_map.get(to) else {
            return Vec::new();
        };
        vec![GraphMutation::AddEdge(GraphEdge {
            id: format!("edge_{source}_{target}"),
            source: source.clone(),
            target: target.clone(),
            edge_type: Some("derived_from".to_string()),
            data: Some(json!({"relationship": event.payload.get("relationship").and_then(Value::as_str).unwrap_or("derived_from")})),
        })]
    }

    fn project_artifact_hidden(&mut self, event: &WorkflowEvent) -> Vec<GraphMutation> {
        let artifact_id = match event.payload.get("artifact_id").and_then(Value::as_str) {
            Some(id) => id,
            None => return Vec::new(),
        };
        self.surface_map
            .remove(artifact_id)
            .map(GraphMutation::RemoveNode)
            .into_iter()
            .collect()
    }
}

fn infer_node_type(format: Option<&str>) -> String {
    match format.unwrap_or_default() {
        "png" | "jpg" | "jpeg" | "svg" => "markdown".to_string(),
        "plotly" | "plotly_json" => "plotly".to_string(),
        "bed" | "bigwig" | "ucf" => "genome_track".to_string(),
        _ => "data_table".to_string(),
    }
}

fn build_node_data(artifact_id: &str, node: Option<&crate::workspace::index::WorkflowNode>) -> Value {
    let label = node
        .and_then(|node| node.summary.clone())
        .unwrap_or_else(|| artifact_id.to_string());
    let mut data = json!({"label": label, "workflow_ref": artifact_id});
    if let Some(preview) = node.and_then(|node| node.preview.as_ref()) {
        match preview {
            ArtifactPreview::Table { columns, rows, total_rows } => {
                data["payload"] = json!({"columns": columns, "rows": rows, "total_rows": total_rows});
            }
            ArtifactPreview::Text { content } => {
                data["content"] = json!(content);
            }
            ArtifactPreview::Plot { spec } => {
                data["payload"] = spec.clone();
            }
            ArtifactPreview::Image { uri, width, height } => {
                data["payload"] = json!({"uri": uri, "width": width, "height": height});
            }
        }
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::types::WorkflowEvent;
    use serde_json::json;

    fn event(kind: WorkflowEventKind, payload: Value) -> WorkflowEvent {
        WorkflowEvent {
            id: "e".to_string(),
            timestamp: "t".to_string(),
            session_id: "s1".to_string(),
            actor: "agent".to_string(),
            kind,
            payload,
        }
    }

    #[test]
    fn surfaced_artifact_becomes_node_with_preview_payload() {
        let produced = event(
            WorkflowEventKind::ArtifactProduced,
            json!({
                "artifact_id":"qc", "summary":"QC table", "format":"tsv", "canvas_kind":"data_table",
                "preview":{"kind":"table", "columns":["sample"], "rows":[["SM30-1"]], "total_rows":1}
            }),
        );
        let index = WorkflowIndex::rebuild(&[produced]);
        let surfaced = event(
            WorkflowEventKind::ArtifactSurfaced,
            json!({"artifact_id":"qc", "canvas_node_id":"node_qc", "position":{"x":10,"y":20}}),
        );
        let mut projector = CanvasProjector::default();
        let mutations = projector.project(&surfaced, &index);
        let GraphMutation::AddNode(node) = &mutations[0] else {
            panic!("expected node");
        };
        assert_eq!(node.id, "node_qc");
        assert_eq!(node.node_type, "data_table");
        assert_eq!(node.data["payload"]["columns"][0], "sample");
    }

    #[test]
    fn edge_only_projects_when_both_artifacts_are_surfaced() {
        let index = WorkflowIndex::default();
        let mut projector = CanvasProjector::default();
        let edge = event(WorkflowEventKind::EdgeDeclared, json!({"from_id":"a", "to_id":"b"}));
        assert!(projector.project(&edge, &index).is_empty());
    }
}
