use crate::workspace::types::{ArtifactPreview, WorkflowEvent, WorkflowEventKind};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowNode {
    pub id: String,
    pub kind: String,
    pub uri: Option<String>,
    pub format: Option<String>,
    pub summary: Option<String>,
    pub canvas_kind: Option<String>,
    pub preview: Option<ArtifactPreview>,
}

#[derive(Clone, Default)]
pub struct WorkflowIndex {
    pub nodes: HashMap<String, WorkflowNode>,
    pub edges: Vec<(String, String, String)>,
}

impl WorkflowIndex {
    pub fn rebuild(events: &[WorkflowEvent]) -> Self {
        let mut index = Self::default();
        for event in events {
            index.apply(event);
        }
        index
    }

    pub fn apply(&mut self, event: &WorkflowEvent) {
        match event.kind {
            WorkflowEventKind::DataIngested => self.apply_data_ingested(&event.payload),
            WorkflowEventKind::StepExecuted => self.apply_step_executed(&event.payload),
            WorkflowEventKind::ArtifactProduced => self.apply_artifact_produced(&event.payload),
            WorkflowEventKind::EdgeDeclared => self.apply_edge_declared(&event.payload),
            _ => {}
        }
    }

    fn apply_data_ingested(&mut self, payload: &Value) {
        if let Some(id) = payload.get("data_id").and_then(Value::as_str) {
            self.nodes.insert(
                id.to_string(),
                WorkflowNode {
                    id: id.to_string(),
                    kind: "data".to_string(),
                    uri: payload
                        .get("uri")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    format: payload
                        .get("format")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    summary: payload
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    canvas_kind: None,
                    preview: None,
                },
            );
        }
    }

    fn apply_step_executed(&mut self, payload: &Value) {
        if let Some(id) = payload.get("step_id").and_then(Value::as_str) {
            self.nodes.insert(
                id.to_string(),
                WorkflowNode {
                    id: id.to_string(),
                    kind: "step".to_string(),
                    uri: None,
                    format: None,
                    summary: payload
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    canvas_kind: None,
                    preview: None,
                },
            );
        }
    }

    fn apply_artifact_produced(&mut self, payload: &Value) {
        if let Some(id) = payload.get("artifact_id").and_then(Value::as_str) {
            let preview = payload
                .get("preview")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok());
            self.nodes.insert(
                id.to_string(),
                WorkflowNode {
                    id: id.to_string(),
                    kind: "artifact".to_string(),
                    uri: payload
                        .get("uri")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    format: payload
                        .get("format")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    summary: payload
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    canvas_kind: payload
                        .get("canvas_kind")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    preview,
                },
            );
            if let Some(step) = payload.get("produced_by").and_then(Value::as_str) {
                self.edges
                    .push((step.to_string(), id.to_string(), "output_of".to_string()));
            }
        }
    }

    fn apply_edge_declared(&mut self, payload: &Value) {
        let from = payload.get("from_id").and_then(Value::as_str);
        let to = payload.get("to_id").and_then(Value::as_str);
        if let (Some(from), Some(to)) = (from, to) {
            let rel = payload
                .get("relationship")
                .and_then(Value::as_str)
                .unwrap_or("derived_from");
            self.edges
                .push((from.to_string(), to.to_string(), rel.to_string()));
        }
    }

    pub fn lineage(&self, target: &str) -> Vec<String> {
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([target.to_string()]);
        while let Some(node) = queue.pop_front() {
            if !seen.insert(node.clone()) {
                continue;
            }
            result.push(node.clone());
            for (from, to, _) in &self.edges {
                if to == &node {
                    queue.push_back(from.clone());
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: &str, kind: WorkflowEventKind, payload: Value) -> WorkflowEvent {
        WorkflowEvent {
            id: id.to_string(),
            timestamp: "t".to_string(),
            session_id: "s1".to_string(),
            actor: "agent".to_string(),
            kind,
            payload,
        }
    }

    #[test]
    fn artifact_produced_preserves_canvas_kind_and_preview() {
        let events = vec![event(
            "e1",
            WorkflowEventKind::ArtifactProduced,
            json!({
                "artifact_id":"qc", "produced_by":"step1", "format":"tsv", "summary":"QC table",
                "canvas_kind":"data_table",
                "preview":{"kind":"table", "columns":["sample"], "rows":[["SM30-1"]], "total_rows":1}
            }),
        )];
        let index = WorkflowIndex::rebuild(&events);
        let node = index.nodes.get("qc").unwrap();
        assert_eq!(node.canvas_kind.as_deref(), Some("data_table"));
        assert!(matches!(node.preview, Some(ArtifactPreview::Table { .. })));
        assert_eq!(
            index.edges,
            vec![(
                "step1".to_string(),
                "qc".to_string(),
                "output_of".to_string()
            )]
        );
    }

    #[test]
    fn lineage_walks_upstream_edges() {
        let events = vec![
            event(
                "e1",
                WorkflowEventKind::EdgeDeclared,
                json!({"from_id":"ucf1", "to_id":"qc", "relationship":"input_to"}),
            ),
            event(
                "e2",
                WorkflowEventKind::EdgeDeclared,
                json!({"from_id":"qc", "to_id":"repro", "relationship":"derived_from"}),
            ),
        ];
        let index = WorkflowIndex::rebuild(&events);
        let lineage = index.lineage("repro");
        assert!(lineage.contains(&"repro".to_string()));
        assert!(lineage.contains(&"qc".to_string()));
        assert!(lineage.contains(&"ucf1".to_string()));
    }
}
