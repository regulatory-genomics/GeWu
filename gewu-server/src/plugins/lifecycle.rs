//! Transactional installation of a release's package and runtime.
//!
//! A runtime may be bundled, reused from the user's machine, or installed into
//! a host-owned prefix. All three converge on the same packed-runtime shape
//! before probe and activation, so verification and rollback stay uniform.

use anyhow::{Context, Result, ensure};
use gewu_protocol::release::{Release, RuntimeProvider, RuntimeRef, Target};
use std::{path::{Path, PathBuf}, time::Duration};

use super::{
    artifacts::{sha256_file, ArtifactStore},
    package::Package,
    providers,
    runtime::{select_runtime, RuntimeBundle},
    state::{GenerationState, RuntimeIdentity, StateStore},
};

/// Which runtime acquisition the caller selected.
pub enum RuntimeSelection<'a> {
    Bundled(&'a RuntimeRef),
    External { provider: &'a RuntimeProvider, executable: Option<&'a Path> },
    Managed { provider: &'a RuntimeProvider },
}

impl<'a> RuntimeSelection<'a> {
    /// Choose a runtime without touching the filesystem. `auto` follows the
    /// plan's declared policy order and falls back to a bundled runtime.
    pub fn select(
        release: &'a Release,
        policy: &str,
        target: &Target,
        executable: Option<&'a Path>,
    ) -> Result<Self> {
        anyhow::ensure!(
            matches!(policy, "auto" | "external" | "managed" | "bundled"),
            "unknown runtime policy {policy}; expected auto, external, managed, or bundled"
        );
        if policy != "bundled"
            && let Some(plan) = &release.runtime_plan
        {
            if policy == "auto" {
                // Prefer the first provider that is actually usable on this
                // host, so auto falls through instead of failing on a stale
                // Python or a missing micromamba.
                for kind in &plan.policies {
                    for provider in plan.providers.iter().filter(|p| p.kind() == kind && p.target() == target) {
                        match provider {
                            RuntimeProvider::External { .. } => {
                                if providers::resolve_external(provider, executable).is_ok() {
                                    return Ok(Self::External { provider, executable });
                                }
                            }
                            RuntimeProvider::Managed { manager, .. } => {
                                if providers::managed_available(manager) {
                                    return Ok(Self::Managed { provider });
                                }
                            }
                            RuntimeProvider::Bundled { .. } => {
                                if let Some(runtime) = release.runtimes.iter().find(|runtime| runtime.target == *target) {
                                    return Ok(Self::Bundled(runtime));
                                }
                            }
                        }
                    }
                }
            } else if let Some(provider) = plan.select(policy, target) {
                return Ok(match provider {
                    RuntimeProvider::External { .. } => Self::External { provider, executable },
                    RuntimeProvider::Managed { .. } => Self::Managed { provider },
                    RuntimeProvider::Bundled { id, .. } => Self::Bundled(
                        release
                            .runtimes
                            .iter()
                            .find(|runtime| &runtime.id == id && runtime.target == *target)
                            .or_else(|| release.runtimes.iter().find(|runtime| runtime.target == *target))
                            .context("bundled provider has no matching runtime reference")?,
                    ),
                });
            } else {
                anyhow::bail!(
                    "release {} has no {policy} runtime for {}",
                    release.release_id,
                    super::runtime::target_label(target)
                );
            }
        }
        let bundled = select_runtime(&release.runtimes, target)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(Self::Bundled(bundled))
    }
}

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

    /// Legacy path: both archives already provided.
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
        self.install_release(
            release,
            package_archive,
            Some(runtime_archive),
            RuntimeSelection::Bundled(runtime),
            workspace_root,
            probe_deadline,
        )
        .await
    }

    pub async fn install_release(
        &self,
        release: &Release,
        package_archive: &Path,
        runtime_archive: Option<&Path>,
        selection: RuntimeSelection<'_>,
        workspace_root: &Path,
        probe_deadline: Duration,
    ) -> Result<GenerationState> {
        let host_target = super::runtime::current_target().map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let package_blob = self.artifacts.put_file(package_archive, &release.package.sha256, release.package.bytes)?;
        let package_sha256 = sha256_file(&package_blob)?;
        let generation_id = release.release_id.clone();

        // Prepare acquisition inputs before staging so a slow download or
        // provider install never leaves a half-written generation behind.
        let mut prefix: Option<PathBuf> = None;
        let mut executable: Option<PathBuf> = None;
        let mut runtime_blob: Option<PathBuf> = None;
        let identity = match &selection {
            RuntimeSelection::Bundled(runtime) => {
                ensure!(runtime.target == host_target, "runtime target does not match host");
                let archive = runtime_archive.ok_or_else(|| anyhow::anyhow!("bundled runtime archive is required"))?;
                let blob = self.artifacts.put_file(archive, &runtime.artifact.sha256, runtime.artifact.bytes)?;
                runtime_blob = Some(blob);
                RuntimeIdentity {
                    provider: runtime.id.clone(),
                    kind: "bundled".into(),
                    executable: None,
                    prefix: None,
                    target: super::runtime::target_label(&host_target),
                }
            }
            RuntimeSelection::External { provider, executable: override_path } => {
                ensure!(provider.target() == &host_target, "runtime provider target does not match host");
                let resolved = providers::resolve_external(provider, *override_path)
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                let probe = match provider {
                    RuntimeProvider::External { probe, .. } => probe,
                    _ => unreachable!("selection is external"),
                };
                providers::probe_executable(&resolved, probe).await
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                executable = Some(resolved);
                RuntimeIdentity {
                    provider: provider.id().into(),
                    kind: "external".into(),
                    executable: executable.as_ref().map(|path| path.display().to_string()),
                    prefix: None,
                    target: super::runtime::target_label(&host_target),
                }
            }
            RuntimeSelection::Managed { provider } => {
                ensure!(provider.target() == &host_target, "runtime provider target does not match host");
                let target_prefix = self.state.root().join("runtimes").join(provider.id()).join(&release.plugin);
                if !providers::is_executable(&target_prefix.join("bin/python3")) {
                    providers::install_managed(provider, &target_prefix, 3600).await
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                }
                prefix = Some(target_prefix);
                RuntimeIdentity {
                    provider: provider.id().into(),
                    kind: "managed".into(),
                    executable: None,
                    prefix: prefix.as_ref().map(|path| path.display().to_string()),
                    target: super::runtime::target_label(&host_target),
                }
            }
        };

        let generation = GenerationState {
            release_id: generation_id.clone(),
            package_path: format!("plugins/{}/generations/{generation_id}/package", release.plugin),
            runtime_path: Some(format!("plugins/{}/generations/{generation_id}/runtime", release.plugin)),
            package_sha256,
            runtime_sha256: None,
            installed_at: release.provenance.created_at.clone(),
            runtime: Some(identity),
        };
        let generation_root = self.state.stage_generation(&release.plugin, &generation, |stage| {
            let package_root = stage.join("package");
            let runtime_root = stage.join("runtime");
            self.artifacts.extract_zip(&package_blob, &package_root)?;
            let package = Package::read(&package_root)?;
            ensure!(
                package.manifest.id == release.plugin && package.manifest.version == release.version,
                "package manifest does not match the release"
            );
            match &selection {
                RuntimeSelection::Bundled(runtime) => {
                    let blob = runtime_blob.as_ref().context("bundled runtime blob is missing")?;
                    self.artifacts.extract_zip(blob, &runtime_root)?;
                    let loaded = RuntimeBundle::load(&runtime_root, &release.plugin, &host_target)
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    let manifest_bytes = std::fs::read(runtime_root.join("runtime.json"))?;
                    ensure!(
                        sha256_bytes(&manifest_bytes) == runtime.manifest.sha256,
                        "runtime manifest digest does not match the release"
                    );
                    ensure!(loaded.manifest.version == runtime.version, "runtime version does not match the release");
                }
                RuntimeSelection::External { provider, .. } => {
                    let resolved = executable.as_ref().context("external runtime was not resolved")?;
                    providers::synthesize_external(&runtime_root, &release.plugin, &release.version, &host_target, provider, resolved)
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                }
                RuntimeSelection::Managed { provider } => {
                    let prefix = prefix.as_ref().context("managed runtime prefix is missing")?;
                    providers::synthesize_managed(&runtime_root, &release.plugin, &release.version, &host_target, provider, prefix)
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                }
            }
            Ok(())
        })?;
        // Persist the resolved runtime before probing so a failed probe can be
        // diagnosed from state without activating the generation.
        let bundle = RuntimeBundle::load(generation_root.join("runtime"), &release.plugin, &host_target)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        if let Err(error) = bundle.probe(&generation_root.join("package"), workspace_root, probe_deadline).await {
            let _ = std::fs::remove_dir_all(&generation_root);
            return Err(anyhow::anyhow!(error.to_string()));
        }
        self.state.commit_active(&release.plugin, generation.clone(), &release.channel)?;
        Ok(generation)
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_protocol::{
        Artifact, ExecutableHint, GeWuCompatibility, ProbeSpec, Provenance, RuntimePlan, RuntimeProvider, RuntimeRef,
    };

    fn target() -> Target {
        Target { os: "linux".into(), arch: "x86_64".into(), libc: Some("glibc".into()) }
    }

    fn artifact(name: &str) -> Artifact {
        Artifact { url: format!("https://example.invalid/{name}"), sha256: "a".repeat(64), bytes: 1 }
    }

    fn release(plan: RuntimePlan, runtimes: Vec<RuntimeRef>) -> Release {
        Release {
            schema_version: "gewu.release/v2".into(),
            release_id: "demo-1.0.0".into(),
            plugin: "demo".into(),
            version: "1.0.0".into(),
            channel: "stable".into(),
            gewu: GeWuCompatibility { api: "gewu.plugin/v1".into(), host: ">=0.1.0".into() },
            package: artifact("demo.package.zip"),
            runtimes,
            runtime_plan: Some(plan),
            capabilities: vec![],
            permissions: vec![],
            provenance: Provenance {
                builder: "test".into(),
                source: "fixture".into(),
                source_commit: "a".repeat(40),
                build_inputs: vec![],
                sbom: None,
                created_at: "2026-11-19T00:00:00Z".into(),
            },
            release_notes: None,
            revocation: None,
        }
    }

    fn external(id: &str, commands: Vec<&str>) -> RuntimeProvider {
        RuntimeProvider::External {
            id: id.into(),
            target: target(),
            executable: ExecutableHint { env: None, commands: commands.into_iter().map(str::to_string).collect() },
            probe: ProbeSpec { args: vec![], timeout_seconds: None },
            binaries: Default::default(),
        }
    }

    #[test]
    fn auto_skips_unavailable_providers_and_uses_bundled() {
        let plan = RuntimePlan {
            schema_version: "gewu.runtime-plan/v1".into(),
            policies: vec!["external".into(), "managed".into(), "bundled".into()],
            providers: vec![
                external("missing-python", vec!["definitely-missing-python-xyz"]),
                RuntimeProvider::Managed {
                    id: "missing-manager".into(),
                    target: target(),
                    manager: "definitely-missing-manager-xyz".into(),
                    channels: vec![],
                    packages: vec!["python".into()],
                    pip: vec![],
                    pip_index: None,
                    pip_extra_index: None,
                    probe: ProbeSpec { args: vec![], timeout_seconds: None },
                    binaries: Default::default(),
                },
                RuntimeProvider::Bundled {
                    id: "demo-runtime".into(),
                    version: "1.0.0".into(),
                    target: target(),
                    artifact: artifact("demo.runtime.zip"),
                    manifest: artifact("demo.runtime.zip.json"),
                },
            ],
        };
        let runtime = RuntimeRef { id: "demo-runtime".into(), version: "1.0.0".into(), target: target(), artifact: artifact("demo.runtime.zip"), manifest: artifact("demo.runtime.zip.json") };
        let release = release(plan, vec![runtime]);
        assert!(matches!(RuntimeSelection::select(&release, "auto", &target(), None).unwrap(), RuntimeSelection::Bundled(_)));
    }

    #[test]
    fn auto_uses_a_usable_external_interpreter() {
        let plan = RuntimePlan {
            schema_version: "gewu.runtime-plan/v1".into(),
            policies: vec!["external".into(), "bundled".into()],
            providers: vec![external("host-python", vec!["python3"])],
        };
        let release = release(plan, vec![]);
        assert!(matches!(RuntimeSelection::select(&release, "auto", &target(), None).unwrap(), RuntimeSelection::External { .. }));
    }
}
