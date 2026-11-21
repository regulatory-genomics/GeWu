//! Packed-runtime selection, validation, and probe contract.

use gewu_protocol::release::{RUNTIME_SCHEMA, RuntimeManifest, RuntimeRef, Target};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fmt,
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;

#[derive(Debug)]
pub enum RuntimeError {
    Missing,
    Incompatible(String),
    Invalid(String),
    ProbeFailed(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("runtime is unavailable"),
            Self::Incompatible(message) => write!(f, "runtime is incompatible: {message}"),
            Self::Invalid(message) => write!(f, "runtime is invalid: {message}"),
            Self::ProbeFailed(message) => write!(f, "runtime probe failed: {message}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

#[derive(Debug, Clone)]
pub struct RuntimeBundle {
    pub root: PathBuf,
    pub manifest: RuntimeManifest,
    pub launcher: PathBuf,
    pub probe: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RuntimeEnvironment {
    pub plugin_root: PathBuf,
    pub runtime_root: PathBuf,
    pub workspace_root: PathBuf,
    pub target: String,
}

impl RuntimeEnvironment {
    pub fn variables(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "GEWU_PLUGIN_ROOT".into(),
                self.plugin_root.display().to_string(),
            ),
            (
                "GEWU_RUNTIME_ROOT".into(),
                self.runtime_root.display().to_string(),
            ),
            (
                "GEWU_WORKSPACE_ROOT".into(),
                self.workspace_root.display().to_string(),
            ),
            ("GEWU_RUNTIME_TARGET".into(), self.target.clone()),
        ])
    }
}

pub fn current_target() -> Result<Target, RuntimeError> {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        return Err(RuntimeError::Incompatible(
            "unsupported operating system".into(),
        ));
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        return Err(RuntimeError::Incompatible(
            "unsupported architecture".into(),
        ));
    };
    let libc = if os == "linux" {
        Some("glibc".into())
    } else {
        None
    };
    Ok(Target {
        os: os.into(),
        arch: arch.into(),
        libc,
    })
}

pub fn target_label(target: &Target) -> String {
    match &target.libc {
        Some(libc) => format!("{}-{}-{libc}", target.os, target.arch),
        None => format!("{}-{}", target.os, target.arch),
    }
}

pub fn select_runtime<'a>(
    runtimes: &'a [RuntimeRef],
    target: &Target,
) -> Result<&'a RuntimeRef, RuntimeError> {
    runtimes
        .iter()
        .find(|runtime| runtime.target == *target)
        .ok_or(RuntimeError::Missing)
}

impl RuntimeBundle {
    pub fn load(
        root: impl Into<PathBuf>,
        expected_plugin: &str,
        expected_target: &Target,
    ) -> std::result::Result<Self, RuntimeError> {
        let root = root
            .into()
            .canonicalize()
            .map_err(|_| RuntimeError::Missing)?;
        let manifest_path = contained(&root, Path::new("runtime.json"))?;
        let bytes =
            std::fs::read(&manifest_path).map_err(|e| RuntimeError::Invalid(e.to_string()))?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|e| RuntimeError::Invalid(e.to_string()))?;
        let schema: Value = serde_json::from_str(RUNTIME_SCHEMA)
            .map_err(|e| RuntimeError::Invalid(e.to_string()))?;
        let validator =
            jsonschema::validator_for(&schema).map_err(|e| RuntimeError::Invalid(e.to_string()))?;
        if let Err(error) = validator.validate(&value) {
            return Err(RuntimeError::Invalid(format!(
                "schema validation failed at {}",
                error.instance_path()
            )));
        }
        let manifest: RuntimeManifest =
            serde_json::from_value(value).map_err(|e| RuntimeError::Invalid(e.to_string()))?;
        if manifest.plugin != expected_plugin {
            return Err(RuntimeError::Invalid(
                "runtime plugin identity does not match package".into(),
            ));
        }
        if manifest.target != *expected_target {
            return Err(RuntimeError::Incompatible(
                "runtime target does not match host".into(),
            ));
        }
        let launcher = contained(&root, Path::new(&manifest.launcher))?;
        let probe = contained(&root, Path::new(&manifest.probe))?;
        if !launcher.is_file() || !probe.is_file() {
            return Err(RuntimeError::Invalid(
                "runtime launcher and probe must be files".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if launcher
                .metadata()
                .map_err(|e| RuntimeError::Invalid(e.to_string()))?
                .permissions()
                .mode()
                & 0o111
                == 0
            {
                return Err(RuntimeError::Invalid(
                    "runtime launcher is not executable".into(),
                ));
            }
            if probe
                .metadata()
                .map_err(|e| RuntimeError::Invalid(e.to_string()))?
                .permissions()
                .mode()
                & 0o111
                == 0
            {
                return Err(RuntimeError::Invalid(
                    "runtime probe is not executable".into(),
                ));
            }
        }
        Ok(Self {
            root,
            manifest,
            launcher,
            probe,
        })
    }

    pub async fn probe(
        &self,
        plugin_root: &Path,
        workspace_root: &Path,
        deadline: Duration,
    ) -> std::result::Result<(), RuntimeError> {
        self.probe_reported(plugin_root, workspace_root, deadline, None)
            .await
    }

    pub(crate) async fn probe_reported(
        &self,
        plugin_root: &Path,
        workspace_root: &Path,
        deadline: Duration,
        report: Option<&mut super::diagnostics::Attempt>,
    ) -> std::result::Result<(), RuntimeError> {
        let environment = RuntimeEnvironment {
            plugin_root: plugin_root.to_path_buf(),
            runtime_root: self.root.clone(),
            workspace_root: workspace_root.to_path_buf(),
            target: target_label(&self.manifest.target),
        };
        let mut command = Command::new(&self.probe);
        command.env_clear().current_dir(workspace_root);
        for key in ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        for (key, value) in environment.variables() {
            command.env(key, value);
        }
        super::diagnostics::run(command, deadline, "runtime probe", report)
            .await
            .map_err(|error| RuntimeError::ProbeFailed(error.to_string()))
    }

    pub fn environment(&self, plugin_root: &Path, workspace_root: &Path) -> RuntimeEnvironment {
        RuntimeEnvironment {
            plugin_root: plugin_root.to_path_buf(),
            runtime_root: self.root.clone(),
            workspace_root: workspace_root.to_path_buf(),
            target: target_label(&self.manifest.target),
        }
    }
}

fn contained(root: &Path, relative: &Path) -> Result<PathBuf, RuntimeError> {
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(RuntimeError::Invalid(
            "runtime path must be contained and relative".into(),
        ));
    }
    let path = root
        .join(relative)
        .canonicalize()
        .map_err(|_| RuntimeError::Missing)?;
    if !path.starts_with(root) {
        return Err(RuntimeError::Invalid(
            "runtime path escapes its root".into(),
        ));
    }
    Ok(path)
}
