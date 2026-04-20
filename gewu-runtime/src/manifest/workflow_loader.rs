//! Loader for workflow manifests.
//!
//! Scans directories for `*.yaml` / `*.yml` files, parses them as
//! `WorkflowManifest`, and returns them. Validation (resolvability of
//! specialist/tool references, DAG reachability, disjoint parallel writes,
//! loops have `max_loops`) lives in `crate::workflow::validate` so that both
//! the CLI load path and the Composer specialist's `write_workflow` tool can
//! share it.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing;

use super::workflow::WorkflowManifest;

pub struct WorkflowManifestLoader {
    dirs: Vec<PathBuf>,
}

impl Default for WorkflowManifestLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkflowManifestLoader {
    pub fn new() -> Self {
        Self { dirs: Vec::new() }
    }

    pub fn add_dir(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        self.dirs.push(dir.into());
        self
    }

    /// Parse every `*.yaml` / `*.yml` file in every registered directory.
    ///
    /// Parse failures are logged and skipped — they don't fail the whole
    /// load. This matches `SpecialistManifestLoader::load` so that one bad
    /// file doesn't break the runtime.
    pub fn load(&self) -> Result<Vec<(PathBuf, WorkflowManifest)>> {
        let mut out = Vec::new();
        for dir in &self.dirs {
            if !dir.exists() {
                tracing::debug!("Workflow manifest dir does not exist, skipping: {}", dir.display());
                continue;
            }
            let entries = std::fs::read_dir(dir)
                .with_context(|| format!("reading workflow manifest dir: {}", dir.display()))?;
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if ext != "yaml" && ext != "yml" {
                    continue;
                }
                match load_workflow_manifest(&path) {
                    Ok(m) => {
                        tracing::info!("Loaded workflow manifest: {} ({})", m.id, path.display());
                        out.push((path, m));
                    }
                    Err(e) => {
                        tracing::warn!("Failed to load workflow manifest {}: {}", path.display(), e);
                    }
                }
            }
        }
        Ok(out)
    }
}

fn load_workflow_manifest(path: &Path) -> Result<WorkflowManifest> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let m: WorkflowManifest = serde_yaml::from_str(&content)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn loads_yaml_files_and_ignores_others() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.yaml"),
            "id: a\nentry: x\nfinish: [x]\nnodes: { x: { kind: tool, tool: t } }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.yml"),
            "id: b\nentry: y\nfinish: [y]\nnodes: { y: { kind: tool, tool: t } }\n",
        )
        .unwrap();
        fs::write(dir.path().join("readme.txt"), "ignore me").unwrap();

        let mut loader = WorkflowManifestLoader::new();
        loader.add_dir(dir.path());
        let loaded = loader.load().unwrap();
        assert_eq!(loaded.len(), 2);
        let mut ids: Vec<_> = loaded.iter().map(|(_, m)| m.id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn missing_dir_is_not_an_error() {
        let mut loader = WorkflowManifestLoader::new();
        loader.add_dir("/tmp/definitely_does_not_exist_gewu_workflow_test");
        let loaded = loader.load().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn malformed_file_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("good.yaml"),
            "id: g\nentry: x\nfinish: [x]\nnodes: { x: { kind: tool, tool: t } }\n").unwrap();
        fs::write(dir.path().join("bad.yaml"), "not: [valid: yaml").unwrap();

        let mut loader = WorkflowManifestLoader::new();
        loader.add_dir(dir.path());
        let loaded = loader.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].1.id, "g");
    }
}
