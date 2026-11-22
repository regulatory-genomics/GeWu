//! Transactional installation of a release's package and runtime.
//!
//! A runtime may be bundled, reused from the user's machine, or installed into
//! a host-owned prefix. All three converge on the same packed-runtime shape
//! before probe and activation, so verification and rollback stay uniform.

use anyhow::{Context, Result, ensure};
use gewu_protocol::release::{Release, RuntimeProvider, RuntimeRef, Target};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use super::{
    artifacts::ArtifactStore,
    diagnostics::Attempt,
    package::Package,
    providers,
    runtime::{RuntimeBundle, select_runtime},
    state::{GenerationState, RuntimeIdentity, StateStore},
};

/// Which runtime acquisition the caller selected.
pub enum RuntimeSelection<'a> {
    Bundled(&'a RuntimeRef),
    External {
        provider: &'a RuntimeProvider,
        executable: Option<PathBuf>,
    },
    Managed {
        provider: &'a RuntimeProvider,
    },
}

impl<'a> RuntimeSelection<'a> {
    /// Enumerate in signed policy order without executing any provider.
    pub fn auto_candidates(
        release: &'a Release,
        target: &Target,
        executable: Option<&Path>,
    ) -> Result<Vec<Self>> {
        let mut candidates = Vec::new();
        if let Some(plan) = &release.runtime_plan {
            for kind in &plan.policies {
                for provider in plan
                    .providers
                    .iter()
                    .filter(|provider| provider.kind() == kind && provider.target() == target)
                {
                    if let Some(candidate) = Self::from_provider(release, provider, executable) {
                        candidates.push(candidate);
                    }
                }
            }
        }
        if !candidates.iter().any(|candidate| matches!(candidate, Self::Bundled(_)))
            && let Ok(runtime) = select_runtime(&release.runtimes, target) {
            candidates.push(Self::Bundled(runtime));
        }
        ensure!(!candidates.is_empty(), "no runtime for {} on {}", release.plugin, super::runtime::target_label(target));
        Ok(candidates)
    }

    fn from_provider(
        release: &'a Release,
        provider: &'a RuntimeProvider,
        executable: Option<&Path>,
    ) -> Option<Self> {
        match provider {
            RuntimeProvider::External { .. } => Some(Self::External { provider, executable:executable.map(Path::to_path_buf) }),
            RuntimeProvider::Managed { .. } => Some(Self::Managed { provider }),
            RuntimeProvider::Bundled { id, target, .. } => release.runtimes.iter()
                .find(|runtime| runtime.id == *id && runtime.target == *target && runtime.profile.as_deref().unwrap_or("core") == "core")
                .map(Self::Bundled),
        }
    }
    /// Plan without running probes or installations; execution tries candidates lazily.
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
        if policy == "auto" {
            return Self::auto_candidates(release, target, executable)?.into_iter()
                .find(|candidate| match candidate {
                    Self::External {provider, executable} => providers::resolve_external(provider, executable.as_deref()).is_ok(),
                    Self::Managed {provider:RuntimeProvider::Managed {manager, ..}} => providers::managed_available(manager),
                    Self::Bundled(_) => true,
                    _ => false,
                }).context("no runtime provider is available on this host");
        }
        if policy != "bundled" && let Some(plan) = &release.runtime_plan {
            return plan.select(policy, target)
                .and_then(|provider| Self::from_provider(release, provider, executable))
                .with_context(|| format!("release {} has no {policy} runtime for {}", release.release_id, super::runtime::target_label(target)));
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
        self.install_reported(
            release,
            package_archive,
            runtime_archive,
            selection,
            workspace_root,
            probe_deadline,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn install_reported(
        &self,
        release: &Release,
        package_archive: &Path,
        runtime_archive: Option<&Path>,
        selection: RuntimeSelection<'_>,
        workspace_root: &Path,
        probe_deadline: Duration,
        mut report: Option<&mut Attempt>,
    ) -> Result<GenerationState> {
        let mut probe_deadline = probe_deadline;
        if let Some(report) = report.as_mut() {
            report.stage("artifact_verify");
        }
        // A second install, removal or rollback of this plugin must not race
        // the staging/probe/commit sequence. Downloads happen before this lock.
        let _mutation = self.state.lock_plugin_mutation(&release.plugin)?;
        let host_target =
            super::runtime::current_target().map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let package_blob = self.artifacts.put_file(
            package_archive,
            &release.package.sha256,
            release.package.bytes,
        )?;
        let generation_id = match &selection {
            RuntimeSelection::Bundled(runtime)
                if matches!(runtime.profile.as_deref(), Some("pdf" | "tex")) =>
            {
                format!(
                    "{}-{}",
                    release.release_id,
                    runtime.profile.as_deref().unwrap()
                )
            }
            _ => release.release_id.clone(),
        };
        let fixed_runtime = if release.schema_version == "gewu.release/v3" {
            let RuntimeSelection::Bundled(runtime) = &selection else {
                anyhow::bail!("release/v3 requires a bundled runtime");
            };
            super::artifacts::validate_digest(&runtime.artifact.sha256)?;
            Some(format!("runtimes/{}", runtime.artifact.sha256))
        } else {
            None
        };

        // Prepare acquisition inputs before staging so a slow download or
        // provider install never leaves a half-written generation behind.
        let mut prefix: Option<PathBuf> = None;
        let mut executable: Option<PathBuf> = None;
        let mut runtime_blob: Option<PathBuf> = None;
        let identity = match &selection {
            RuntimeSelection::Bundled(runtime) => {
                ensure!(
                    runtime.target == host_target,
                    "runtime target does not match host"
                );
                let archive = runtime_archive
                    .ok_or_else(|| anyhow::anyhow!("bundled runtime archive is required"))?;
                let blob = self.artifacts.put_file(
                    archive,
                    &runtime.artifact.sha256,
                    runtime.artifact.bytes,
                )?;
                runtime_blob = Some(blob);
                RuntimeIdentity {
                    provider: runtime.id.clone(),
                    kind: "bundled".into(),
                    executable: None,
                    prefix: None,
                    target: super::runtime::target_label(&host_target),
                }
            }
            RuntimeSelection::External {
                provider,
                executable: override_path,
            } => {
                if let Some(report) = report.as_mut() {
                    report.stage("runtime_acquisition");
                }
                ensure!(
                    provider.target() == &host_target,
                    "runtime provider target does not match host"
                );
                let resolved = providers::resolve_external(provider, override_path.as_deref())
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                let probe = match provider {
                    RuntimeProvider::External { probe, .. } => probe,
                    _ => unreachable!("selection is external"),
                };
                probe_deadline = Duration::from_secs(probe.timeout_seconds.unwrap_or(120));
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
                if let Some(report) = report.as_mut() {
                    report.stage("runtime_acquisition");
                }
                ensure!(
                    provider.target() == &host_target,
                    "runtime provider target does not match host"
                );
                let key = providers::managed_prefix_key(&release.plugin, provider)
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                let target_prefix = self.state.root().join("runtimes").join(key);
                if !providers::managed_complete(&target_prefix) {
                    providers::install_managed_reported(
                        provider,
                        &target_prefix,
                        3600,
                        report.as_deref_mut(),
                    )
                    .await
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
            package_path: format!(
                "plugins/{}/generations/{generation_id}/package",
                release.plugin
            ),
            runtime_path: Some(fixed_runtime.clone().unwrap_or_else(|| {
                format!(
                    "plugins/{}/generations/{generation_id}/runtime",
                    release.plugin
                )
            })),
            package_sha256: release.package.sha256.clone(),
            runtime_sha256: match &selection {
                RuntimeSelection::Bundled(r) => Some(r.artifact.sha256.clone()),
                _ => None,
            },
            installed_at: release.provenance.created_at.clone(),
            runtime: Some(identity),
        };
        if let Some(report) = report.as_mut() {
            report.stage("extract");
        }
        let (generation_root, stage) = self
            .state
            .prepare_generation(&release.plugin, &generation)?;
        let stage = {
            let stage = stage.path();
            let package_root = stage.join("package");
            let runtime_root = stage.join("runtime");
            self.artifacts.extract_zip(&package_blob, &package_root)?;
            let package = Package::read(&package_root)?;
            ensure!(
                package.manifest.id == release.plugin
                    && package.manifest.version == release.version,
                "package manifest does not match the release"
            );
            match &selection {
                RuntimeSelection::Bundled(_) if fixed_runtime.is_some() => {}
                RuntimeSelection::Bundled(runtime) => {
                    let blob = runtime_blob
                        .as_ref()
                        .context("bundled runtime blob is missing")?;
                    self.artifacts.extract_zip(blob, &runtime_root)?;
                    let loaded = RuntimeBundle::load(&runtime_root, &release.plugin, &host_target)
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    check_bundle(&loaded, runtime, "gewu.runtime/v1")?;
                }
                RuntimeSelection::External { provider, .. } => {
                    let resolved = executable
                        .as_ref()
                        .context("external runtime was not resolved")?;
                    providers::synthesize_external(
                        &runtime_root,
                        &release.plugin,
                        &release.version,
                        &host_target,
                        provider,
                        resolved,
                    )
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                }
                RuntimeSelection::Managed { provider } => {
                    let prefix = prefix
                        .as_ref()
                        .context("managed runtime prefix is missing")?;
                    providers::synthesize_managed(
                        &runtime_root,
                        &release.plugin,
                        &release.version,
                        &host_target,
                        provider,
                        prefix,
                    )
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                }
            }
            stage
        };
        let bundle = if let (Some(relative), RuntimeSelection::Bundled(runtime)) =
            (&fixed_runtime, &selection)
        {
            if let Some(report) = report.as_mut() {
                report.stage("runtime_preparation");
            }
            let root = self.state.root().join(relative);
            let _lock = super::state::lock_file_async(root.with_extension("prepare.lock")).await?;
            let receipt = root.with_extension("prepared.json");
            let complete = if receipt.exists() && root.exists() {
                let prefix: PathBuf = serde_json::from_slice(&std::fs::read(&receipt)?)?;
                ensure!(
                    prefix == root.canonicalize()?,
                    "prepared runtime was moved; restore the original installation path"
                );
                true
            } else {
                false
            };
            if !complete {
                if receipt.exists() {
                    std::fs::remove_file(&receipt)?;
                }
                if root.exists() {
                    std::fs::remove_dir_all(&root)?;
                }
                self.artifacts.extract_zip(
                    runtime_blob.as_deref().context("missing runtime blob")?,
                    &root,
                )?;
            }
            let bundle = RuntimeBundle::load(&root, &release.plugin, &host_target)?;
            check_bundle(&bundle, runtime, "gewu.runtime/v2")?;
            if !complete {
                bundle.prepare(report.as_deref_mut()).await?;
                super::state::atomic_json(&receipt, &bundle.root)?;
            }
            bundle
        } else {
            RuntimeBundle::load(stage.join("runtime"), &release.plugin, &host_target)?
        };
        if let Some(report) = report.as_mut() {
            report.stage("runtime_probe");
        }
        let smoke = fixed_runtime
            .as_ref()
            .map(|_| tempfile::tempdir())
            .transpose()?;
        bundle
            .probe_reported(
                &stage.join("package"),
                smoke
                    .as_ref()
                    .map_or(workspace_root, tempfile::TempDir::path),
                probe_deadline,
                report.as_deref_mut(),
            )
            .await?;
        if let Some(report) = report.as_mut() {
            report.stage("commit");
        }
        self.state.publish_generation(
            &release.plugin,
            generation.clone(),
            &release.channel,
            stage,
            &generation_root,
        )?;
        Ok(generation)
    }
}

fn check_bundle(bundle: &RuntimeBundle, runtime: &RuntimeRef, schema: &str) -> Result<()> {
    let manifest = std::fs::read(bundle.root.join("runtime.json"))?;
    ensure!(
        manifest.len() as u64 == runtime.manifest.bytes
            && super::artifacts::sha256_bytes(&manifest) == runtime.manifest.sha256,
        "runtime manifest digest or length does not match the release"
    );
    ensure!(
        bundle.manifest.version == runtime.version && bundle.manifest.id == runtime.id,
        "runtime identity does not match the release"
    );
    ensure!(
        bundle.manifest.schema_version == schema,
        "runtime schema does not match the release"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_protocol::{
        Artifact, ExecutableHint, GeWuCompatibility, ProbeSpec, Provenance, RuntimePlan,
        RuntimeProvider, RuntimeRef,
    };

    fn target() -> Target {
        Target {
            os: "linux".into(),
            arch: "x86_64".into(),
            libc: Some("glibc".into()),
        }
    }

    fn artifact(name: &str) -> Artifact {
        Artifact {
            url: format!("https://example.invalid/{name}"),
            sha256: "a".repeat(64),
            bytes: 1,
        }
    }

    fn release(plan: RuntimePlan, runtimes: Vec<RuntimeRef>) -> Release {
        Release {
            schema_version: "gewu.release/v2".into(),
            release_id: "demo-1.0.0".into(),
            plugin: "demo".into(),
            version: "1.0.0".into(),
            channel: "stable".into(),
            gewu: GeWuCompatibility {
                api: "gewu.plugin/v1".into(),
                host: ">=0.1.0".into(),
            },
            package: artifact("demo.package.zip"),
            runtimes,
            runtime_plan: Some(plan),
            required_features: vec![],
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
            executable: ExecutableHint {
                env: None,
                commands: commands.into_iter().map(str::to_string).collect(),
            },
            probe: ProbeSpec {
                args: vec![],
                timeout_seconds: None,
            },
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
                    probe: ProbeSpec {
                        args: vec![],
                        timeout_seconds: None,
                    },
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
        let runtime = RuntimeRef {
            id: "demo-runtime".into(),
            version: "1.0.0".into(),
            target: target(),
            artifact: artifact("demo.runtime.zip"),
            manifest: artifact("demo.runtime.zip.json"),
            runtime_schema: None,
            profile: None,
            requirements: None,
            unpacked_bytes: None,
            installed_bytes_upper_bound: None,
        };
        let release = release(plan, vec![runtime]);
        assert!(matches!(
            RuntimeSelection::select(&release, "auto", &target(), None).unwrap(),
            RuntimeSelection::Bundled(_)
        ));
    }

    #[test]
    fn auto_uses_a_usable_external_interpreter() {
        let plan = RuntimePlan {
            schema_version: "gewu.runtime-plan/v1".into(),
            policies: vec!["external".into(), "bundled".into()],
            providers: vec![external("host-python", vec!["python3"])],
        };
        let release = release(plan, vec![]);
        assert!(matches!(
            RuntimeSelection::select(&release, "auto", &target(), None).unwrap(),
            RuntimeSelection::External { .. }
        ));
    }

    #[test]
    fn enumerating_candidates_does_not_probe_an_unused_external_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("unused-fallback");
        let mut fallback = external("fallback", vec!["python3"]);
        if let RuntimeProvider::External { probe, .. } = &mut fallback {
            probe.args = vec!["-c".into(), format!("open({:?}, 'w').write('ran')", marker.to_str().unwrap())];
        }
        let runtime = RuntimeRef {
            id:"demo-runtime".into(), version:"1.0.0".into(), target:target(),
            artifact:artifact("runtime.zip"), manifest:artifact("runtime.json"), runtime_schema:None,
            profile:None, requirements:None, unpacked_bytes:None, installed_bytes_upper_bound:None,
        };
        let bundled = RuntimeProvider::Bundled { id:runtime.id.clone(), version:runtime.version.clone(),
            target:runtime.target.clone(), artifact:runtime.artifact.clone(), manifest:runtime.manifest.clone() };
        let release = release(RuntimePlan {schema_version:"gewu.runtime-plan/v1".into(),
            policies:vec!["bundled".into(), "external".into()], providers:vec![bundled, fallback]}, vec![runtime]);
        let candidates = RuntimeSelection::auto_candidates(&release, &target(), None).unwrap();
        assert!(matches!(candidates[0], RuntimeSelection::Bundled(_)));
        assert!(!marker.exists(), "enumeration executed a provider that may never be tried");
    }
}
