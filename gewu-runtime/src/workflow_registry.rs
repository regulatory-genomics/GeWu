//! Aggregate workflow registry.
//!
//! Collects `WorkflowManifest`s from:
//!   - each plugin's `Plugin::workflow_manifest_dir()`
//!   - a caller-supplied list of extra dirs (CLI / user home / project dirs)
//!
//! The result is a name-keyed `HashMap<String, WorkflowManifest>` that
//! `WorkflowCompiler::with_workflows` consumes.

use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::manifest::workflow::WorkflowManifest;
use crate::manifest::workflow_loader::WorkflowManifestLoader;
use crate::plugin_host::PluginHost;

#[derive(Default)]
pub struct WorkflowRegistry {
    manifests: HashMap<String, WorkflowManifest>,
}

impl WorkflowRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_sources(plugin_host: &PluginHost, extra_dirs: &[PathBuf]) -> Result<Self> {
        let mut loader = WorkflowManifestLoader::new();
        for plugin in plugin_host.plugins() {
            if let Some(d) = plugin.workflow_manifest_dir() {
                loader.add_dir(d);
            }
        }
        for d in extra_dirs {
            loader.add_dir(d);
        }
        let loaded = loader.load()?;
        let mut manifests: HashMap<String, WorkflowManifest> = HashMap::new();
        for (path, m) in loaded {
            if let Some(prev) = manifests.insert(m.id.clone(), m.clone()) {
                tracing::warn!(
                    "Workflow id collision: '{}' from {} overrides prior '{}'",
                    m.id,
                    path.display(),
                    prev.id
                );
            }
        }
        Ok(Self { manifests })
    }

    pub fn insert(&mut self, m: WorkflowManifest) {
        self.manifests.insert(m.id.clone(), m);
    }

    pub fn get(&self, id: &str) -> Option<&WorkflowManifest> {
        self.manifests.get(id)
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.manifests.keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn into_map(self) -> HashMap<String, WorkflowManifest> {
        self.manifests
    }

    pub fn as_map(&self) -> &HashMap<String, WorkflowManifest> {
        &self.manifests
    }
}
