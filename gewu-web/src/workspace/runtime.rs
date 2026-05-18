use crate::workspace::{
    CanvasProjector, GraphMutation, SessionGraphStore, WorkflowEventStore, WorkflowIndex,
};
use anyhow::Result;
use std::path::PathBuf;
use tokio::sync::{Mutex, broadcast};

#[derive(Clone, Debug)]
pub struct SessionGraphMutation {
    pub session_id: String,
    pub mutation: GraphMutation,
}

pub struct WorkspaceRuntime {
    pub workspace_id: String,
    pub project_root: PathBuf,
    pub storage_dir: PathBuf,
    pub workflow_store: Mutex<WorkflowEventStore>,
    pub workflow_index: Mutex<WorkflowIndex>,
    pub graph_store: SessionGraphStore,
    pub projector: Mutex<CanvasProjector>,
    pub graph_events: broadcast::Sender<SessionGraphMutation>,
}

impl WorkspaceRuntime {
    pub fn open(
        workspace_id: impl Into<String>,
        project_root: PathBuf,
        storage_dir: PathBuf,
    ) -> Result<Self> {
        let workspace_id = workspace_id.into();
        let workflow_store = WorkflowEventStore::open(storage_dir.join("workflow.jsonl"))?;
        let workflow_index = WorkflowIndex::rebuild(workflow_store.all());
        let graph_store = SessionGraphStore::new(&storage_dir);
        let projector = CanvasProjector::rebuild_from_events(workflow_store.all());
        let (graph_events, _) = broadcast::channel(256);
        Ok(Self {
            workspace_id,
            project_root,
            storage_dir,
            workflow_store: Mutex::new(workflow_store),
            workflow_index: Mutex::new(workflow_index),
            graph_store,
            projector: Mutex::new(projector),
            graph_events,
        })
    }

    pub async fn index(&self) -> WorkflowIndex {
        self.workflow_index.lock().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn runtime_opens_empty_workspace_index() {
        let dir = tempdir().unwrap();
        let runtime = WorkspaceRuntime::open(
            "default",
            dir.path().to_path_buf(),
            dir.path().join(".tachyon"),
        )
        .unwrap();
        assert_eq!(runtime.workspace_id, "default");
        assert!(runtime.index().await.nodes.is_empty());
    }
}
