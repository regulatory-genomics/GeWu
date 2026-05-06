//! Loader for YAML runtime plugin manifests.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::manifest::schema::RuntimePluginManifest;

#[derive(Debug, Clone)]
pub struct LoadedRuntimePluginManifest {
    pub manifest: RuntimePluginManifest,
    pub source_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct RuntimePluginManifestLoader {
    dirs: Vec<PathBuf>,
}

impl RuntimePluginManifestLoader {
    pub fn new() -> Self {
        Self { dirs: Vec::new() }
    }

    pub fn with_default_dirs() -> Self {
        let mut loader = Self::new();
        loader.add_default_dirs();
        loader
    }

    pub fn add_dir(&mut self, dir: impl Into<PathBuf>) {
        let dir = dir.into();
        if !self.dirs.iter().any(|existing| existing == &dir) {
            self.dirs.push(dir);
        }
    }

    pub fn add_default_dirs(&mut self) {
        let project_plugins = PathBuf::from(".gewu/runtime-plugins");
        self.add_dir(project_plugins);

        let home_plugins = dirs::home_dir()
            .map(|h| h.join(".gewu").join("runtime-plugins"))
            .unwrap_or_else(|| PathBuf::from(".gewu/runtime-plugins"));
        self.add_dir(home_plugins);
    }

    pub fn load(&self) -> Result<Vec<LoadedRuntimePluginManifest>> {
        let mut loaded = Vec::new();
        for dir in &self.dirs {
            if !dir.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(dir)
                .with_context(|| format!("reading runtime plugin dir {}", dir.display()))?
            {
                let path = entry?.path();
                if !is_yaml(&path) {
                    continue;
                }
                let content = std::fs::read_to_string(&path).with_context(|| {
                    format!("reading runtime plugin manifest {}", path.display())
                })?;
                let manifest: RuntimePluginManifest =
                    serde_yaml::from_str(&content).with_context(|| {
                        format!("parsing runtime plugin manifest {}", path.display())
                    })?;
                if !manifest.enabled {
                    tracing::debug!(
                        "Skipping disabled runtime plugin manifest {}",
                        path.display()
                    );
                    continue;
                }
                loaded.push(LoadedRuntimePluginManifest {
                    manifest,
                    source_path: path,
                });
            }
        }
        Ok(loaded)
    }
}

fn is_yaml(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e == "yaml" || e == "yml")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_skips_disabled_plugins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("disabled.yaml"),
            r#"
id: disabled
kind: http
enabled: false
base_url: http://127.0.0.1:7878
"#,
        )
        .unwrap();

        let mut loader = RuntimePluginManifestLoader::new();
        loader.add_dir(dir.path());
        let loaded = loader.load().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn loader_reads_enabled_plugins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("memory.yaml"),
            r#"
id: gewu-memory
kind: http
base_url: http://127.0.0.1:7878
tools:
  - name: memory_search
    endpoint: /v1/memory/search
hooks:
  - name: prepare_context
    event: specialist.system_prompt.transform
    endpoint: /v1/context/prepare
"#,
        )
        .unwrap();

        let mut loader = RuntimePluginManifestLoader::new();
        loader.add_dir(dir.path());
        let loaded = loader.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].manifest.id, "gewu-memory");
        assert_eq!(loaded[0].manifest.tools[0].name, "memory_search");
    }
}
