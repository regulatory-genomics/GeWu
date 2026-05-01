//! Manifest loader: scans directories for YAML specialist and tool-provider manifests,
//! validates them against a `CapabilityRegistry`, and converts them into `SpecialistDescriptor`s.

use anyhow::{anyhow, Context, Result};
use gewu_core::types::SpecialistDescriptor;
use std::path::{Path, PathBuf};
use tracing;

use super::schema::SpecialistManifest;
use crate::capability::CapabilityRegistry;

/// Loads specialist manifests from one or more directories.
pub struct SpecialistManifestLoader {
    dirs: Vec<PathBuf>,
}

impl SpecialistManifestLoader {
    pub fn new() -> Self {
        Self { dirs: Vec::new() }
    }

    pub fn add_dir(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        self.dirs.push(dir.into());
        self
    }

    /// Scan all registered directories for `*.yaml` / `*.yml` files, parse and
    /// validate them, and return `SpecialistDescriptor`s.
    ///
    /// If `registry` is `Some`, each capability reference is validated; unknown
    /// capabilities cause a warning (not a hard error) so that manifests can be
    /// loaded even before all providers are registered.
    pub fn load(&self, registry: Option<&CapabilityRegistry>) -> Result<Vec<SpecialistDescriptor>> {
        let mut descriptors = Vec::new();
        for dir in &self.dirs {
            if !dir.exists() {
                tracing::debug!("Manifest dir does not exist, skipping: {}", dir.display());
                continue;
            }
            let entries = std::fs::read_dir(dir)
                .with_context(|| format!("reading manifest dir: {}", dir.display()))?;
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if ext != "yaml" && ext != "yml" {
                    continue;
                }
                match load_specialist_manifest(&path, registry) {
                    Ok(desc) => {
                        tracing::info!(
                            "Loaded specialist manifest: {} ({})",
                            desc.id,
                            path.display()
                        );
                        descriptors.push(desc);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to load manifest {}: {}", path.display(), e);
                    }
                }
            }
        }
        Ok(descriptors)
    }
}

fn load_specialist_manifest(
    path: &Path,
    registry: Option<&CapabilityRegistry>,
) -> Result<SpecialistDescriptor> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let manifest: SpecialistManifest =
        serde_yaml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;

    let base_dir = path
        .parent()
        .ok_or_else(|| anyhow!("manifest path has no parent: {}", path.display()))?;

    let system_prompt = manifest
        .prompt
        .resolve(base_dir)
        .with_context(|| format!("resolving prompt for specialist '{}'", manifest.id))?;

    if let Some(reg) = registry {
        for cap in &manifest.capabilities {
            if !reg.contains(cap) {
                tracing::warn!(
                    "Specialist '{}' references unknown capability '{}' \
                     (manifest: {})",
                    manifest.id,
                    cap,
                    path.display()
                );
            }
        }
    }

    Ok(SpecialistDescriptor {
        id: manifest.id,
        description: manifest.description,
        capabilities: manifest.capabilities,
        skills: manifest.skills,
        system_prompt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn load_from_directory() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = r#"
id: test-spec
description: Test specialist
prompt:
  inline: "Hello"
capabilities:
  - alpha
"#;
        fs::write(dir.path().join("test-spec.yaml"), manifest).unwrap();
        // non-yaml file should be ignored
        fs::write(dir.path().join("readme.txt"), "ignore me").unwrap();

        let mut loader = SpecialistManifestLoader::new();
        loader.add_dir(dir.path());
        let descs = loader.load(None).unwrap();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].id, "test-spec");
        assert_eq!(descs[0].system_prompt, "Hello");
    }

    #[test]
    fn load_with_file_prompt() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("prompts")).unwrap();
        fs::write(dir.path().join("prompts/my.txt"), "I am the prompt.").unwrap();
        let manifest = r#"
id: file-spec
description: File prompt
prompt:
  file: ./prompts/my.txt
capabilities: []
"#;
        fs::write(dir.path().join("file-spec.yaml"), manifest).unwrap();

        let mut loader = SpecialistManifestLoader::new();
        loader.add_dir(dir.path());
        let descs = loader.load(None).unwrap();
        assert_eq!(descs[0].system_prompt, "I am the prompt.");
    }

    #[test]
    fn missing_dir_is_not_an_error() {
        let mut loader = SpecialistManifestLoader::new();
        loader.add_dir("/tmp/definitely_does_not_exist_gewu_test");
        let descs = loader.load(None).unwrap();
        assert!(descs.is_empty());
    }
}
