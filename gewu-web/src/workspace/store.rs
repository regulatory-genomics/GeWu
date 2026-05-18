use crate::workspace::types::{SessionGraph, WorkflowEvent, WorkflowEventKind};
use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

pub struct WorkflowEventStore {
    path: PathBuf,
    events: Vec<WorkflowEvent>,
}

impl WorkflowEventStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut events = Vec::new();
        if path.exists() {
            let content = fs::read_to_string(&path)?;
            for line in content.lines().filter(|line| !line.trim().is_empty()) {
                let event: WorkflowEvent = serde_json::from_str(line)
                    .with_context(|| format!("invalid workflow event JSONL line: {line}"))?;
                events.push(event);
            }
        }
        Ok(Self { path, events })
    }

    pub fn append(&mut self, event: WorkflowEvent) -> Result<()> {
        let line = serde_json::to_string(&event)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{line}")?;
        self.events.push(event);
        Ok(())
    }

    pub fn all(&self) -> &[WorkflowEvent] {
        &self.events
    }

    pub fn by_kind(&self, kind: WorkflowEventKind) -> Vec<&WorkflowEvent> {
        self.events
            .iter()
            .filter(|event| event.kind == kind)
            .collect()
    }
}

#[derive(Clone)]
pub struct SessionGraphStore {
    root: PathBuf,
}

impl SessionGraphStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn graph_path(&self, session_id: &str) -> PathBuf {
        self.root.join("sessions").join(session_id).join("graph.json")
    }

    pub fn load(&self, session_id: &str) -> Result<SessionGraph> {
        let path = self.graph_path(session_id);
        if !path.exists() {
            return Ok(SessionGraph::empty(session_id));
        }
        let text = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn save(&self, graph: &SessionGraph) -> Result<()> {
        let path = self.graph_path(&graph.session_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string_pretty(graph)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::types::{GraphNode, GraphPosition};
    use serde_json::json;
    use tempfile::tempdir;

    fn event(id: &str, kind: WorkflowEventKind) -> WorkflowEvent {
        WorkflowEvent {
            id: id.to_string(),
            timestamp: "2026-05-18T00:00:00Z".to_string(),
            session_id: "s1".to_string(),
            actor: "agent".to_string(),
            kind,
            payload: json!({"artifact_id":"a1"}),
        }
    }

    #[test]
    fn workflow_store_appends_and_reloads_jsonl_events() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("workflow.jsonl");
        let mut store = WorkflowEventStore::open(&path).unwrap();
        store
            .append(event("e1", WorkflowEventKind::ArtifactProduced))
            .unwrap();
        drop(store);

        let reloaded = WorkflowEventStore::open(&path).unwrap();
        assert_eq!(reloaded.all().len(), 1);
        assert_eq!(reloaded.all()[0].id, "e1");
        assert_eq!(
            reloaded.by_kind(WorkflowEventKind::ArtifactProduced).len(),
            1
        );
    }

    #[test]
    fn session_graph_store_is_session_scoped() {
        let dir = tempdir().unwrap();
        let store = SessionGraphStore::new(dir.path());
        let mut graph = SessionGraph::empty("s1");
        graph.nodes.push(GraphNode {
            id: "n1".to_string(),
            node_type: "data_table".to_string(),
            position: GraphPosition { x: 0.0, y: 0.0 },
            data: json!({"label":"QC"}),
        });
        store.save(&graph).unwrap();

        assert_eq!(store.load("s1").unwrap().nodes.len(), 1);
        assert_eq!(store.load("s2").unwrap().nodes.len(), 0);
    }
}
