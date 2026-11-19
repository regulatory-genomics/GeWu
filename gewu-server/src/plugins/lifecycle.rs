//! Transactional installation of a release's package and packed runtime.

use anyhow::{Result, ensure};
use gewu_protocol::release::{Release, Target};
use std::{path::{Path, PathBuf}, time::Duration};

use super::{
    artifacts::{sha256_file, ArtifactStore},
    package::Package,
    runtime::{select_runtime, RuntimeBundle},
    state::{GenerationState, StateStore},
};

#[derive(Clone, Debug)]
pub struct InstallManager {
    pub artifacts: ArtifactStore,
    pub state: StateStore,
}

impl InstallManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            artifacts: ArtifactStore::new(root.join("store")),
            state: StateStore::new(root),
        }
    }

    pub async fn install_archives(
        &self,
        release: &Release,
        package_archive: &Path,
        runtime_archive: &Path,
        target: &Target,
        workspace_root: &Path,
        probe_deadline: Duration,
    ) -> Result<GenerationState> {
        let runtime = select_runtime(&release.runtimes, target)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let package_blob = self.artifacts.put_file(
            package_archive,
            &release.package.sha256,
            release.package.bytes,
        )?;
        let runtime_blob = self.artifacts.put_file(
            runtime_archive,
            &runtime.artifact.sha256,
            runtime.artifact.bytes,
        )?;
        let generation_id = release.release_id.clone();
        let generation = GenerationState {
            release_id: generation_id.clone(),
            package_path: format!("plugins/{}/generations/{generation_id}/package", release.plugin),
            runtime_path: Some(format!("plugins/{}/generations/{generation_id}/runtime", release.plugin)),
            package_sha256: sha256_file(&package_blob)?,
            runtime_sha256: Some(sha256_file(&runtime_blob)?),
            installed_at: release.provenance.created_at.clone(),
        };
        let generation_root = self.state.stage_generation(&release.plugin, &generation, |stage| {
            let package_root = stage.join("package");
            let runtime_root = stage.join("runtime");
            self.artifacts.extract_zip(&package_blob, &package_root)?;
            self.artifacts.extract_zip(&runtime_blob, &runtime_root)?;
            let package = Package::read(&package_root)?;
            ensure!(
                package.manifest.id == release.plugin && package.manifest.version == release.version,
                "package manifest does not match the release"
            );
            let host_target = super::runtime::current_target()
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            let loaded = RuntimeBundle::load(&runtime_root, &release.plugin, &host_target)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            let manifest_bytes = std::fs::read(runtime_root.join("runtime.json"))?;
            ensure!(
                sha256_bytes(&manifest_bytes) == runtime.manifest.sha256,
                "runtime manifest digest does not match the release"
            );
            ensure!(loaded.manifest.version == runtime.version, "runtime version does not match the release");
            Ok(())
        })?;
        let host_target = super::runtime::current_target()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let bundle = RuntimeBundle::load(generation_root.join("runtime"), &release.plugin, &host_target)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        if let Err(error) = bundle
            .probe(&generation_root.join("package"), workspace_root, probe_deadline)
            .await
        {
            let _ = std::fs::remove_dir_all(&generation_root);
            return Err(anyhow::anyhow!(error.to_string()));
        }
        self.state
            .commit_active(&release.plugin, generation.clone(), &release.channel)?;
        Ok(generation)
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}
