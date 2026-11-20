//! Package validation is shared by activation and the local installer.
use super::runtime::RuntimeEnvironment;
use anyhow::{Context, Result, anyhow, ensure};
use gewu_protocol::plugin::{Backend, Manifest, Object, SCHEMA, ViewKind};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
};
use tokio::process::Command;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginEntry {
    /// An empty package path means "use the active marketplace generation".
    #[serde(default)]
    pub package: PathBuf,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub settings: Object,
    #[serde(default)]
    pub workspace_settings: BTreeMap<String, Object>,
    #[serde(default)]
    pub service_origins: Vec<String>,
    #[serde(default)]
    pub sources: BTreeMap<String, PathBuf>,
}

impl std::fmt::Debug for PluginEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginEntry")
            .field("package", &self.package)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct Package {
    pub root: PathBuf,
    pub manifest: Manifest,
    executable: PathBuf,
    pub(crate) skills: Vec<gewu_tools::SkillInfo>,
}

#[derive(Clone, Debug)]
pub struct PackageIssue {
    pub root: PathBuf,
    pub error: String,
}

pub fn validator(schema: &Value) -> Result<jsonschema::Validator> {
    // No network/filesystem retriever: embedded $defs and local document refs only.
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(schema)
        .map_err(|_| anyhow!("invalid or unresolved JSON Schema"))
}

pub fn validate(validator: &jsonschema::Validator, value: &Value) -> Result<()> {
    validator
        .validate(value)
        .map_err(|err| anyhow!("schema validation failed at {}", err.instance_path()))
}

pub fn wire_validator(definition: &str) -> Result<jsonschema::Validator> {
    let mut schema: Value = serde_json::from_str(SCHEMA)?;
    schema["$ref"] = format!("#/$defs/{definition}").into();
    validator(&schema)
}

pub fn contained(root: &Path, path: &Path) -> Result<PathBuf> {
    let candidate = root.join(path);
    let path = candidate.canonicalize().with_context(|| {
        format!(
            "path does not exist: {} (workspace root: {})",
            candidate.display(),
            root.display()
        )
    })?;
    ensure!(
        path.starts_with(root),
        "path escapes its root: {} is outside {}",
        path.display(),
        root.display()
    );
    Ok(path)
}

pub fn relative(path: &str) -> Result<&Path> {
    let path = Path::new(path);
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "expected a contained relative path"
    );
    Ok(path)
}

impl Package {
    pub fn read(path: &Path) -> Result<Self> {
        let root = path
            .canonicalize()
            .context("could not locate plugin package")?;
        let manifest_path = contained(&root, Path::new("plugin.json"))?;
        ensure!(
            manifest_path.is_file() && manifest_path.metadata()?.len() <= 1024 * 1024,
            "invalid plugin.json file"
        );
        let wire: Value = serde_json::from_slice(&fs::read(manifest_path)?)?;
        validate(&wire_validator("Manifest")?, &wire)?;
        let manifest: Manifest = serde_json::from_value(wire)?;
        let Backend::Stdio { entry, .. } = &manifest.backend;
        let executable = contained(&root, relative(entry)?)?;
        ensure!(executable.is_file(), "plugin entry is not a file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                executable.metadata()?.permissions().mode() & 0o111 != 0,
                "plugin entry is not executable"
            );
        }
        let mut ids = HashSet::new();
        for view in &manifest.views {
            ensure!(ids.insert(view.id.clone()), "duplicate view id");
            if let ViewKind::Webview {
                entry,
                state_schema,
            } = &view.kind
            {
                ensure!(
                    contained(&root, relative(entry)?)?.is_file(),
                    "plugin view is not a file"
                );
                validator(&Value::Object(state_schema.clone()))?;
            }
        }
        if let Some(schema) = &manifest.settings_schema {
            validator(&Value::Object(schema.clone()))?;
        }
        let skills = if let Ok(dir) = contained(&root, Path::new("skills")) {
            gewu_tools::skill::discover(&[dir])
                .into_iter()
                .map(|mut skill| {
                    skill.name = format!("{}:{}", manifest.id, skill.name);
                    skill
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(Self {
            root,
            manifest,
            executable,
            skills,
        })
    }

    pub fn validate_settings(&self, settings: &Object) -> Result<()> {
        if let Some(schema) = &self.manifest.settings_schema {
            validate(
                &validator(&Value::Object(schema.clone()))?,
                &Value::Object(settings.clone()),
            )?;
        }
        Ok(())
    }

    pub fn command(&self, cwd: &Path) -> Command {
        let Backend::Stdio { args, .. } = &self.manifest.backend;
        let mut command = Command::new(&self.executable);
        command.args(args).current_dir(cwd).env_clear();
        for key in ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR", "SYSTEMROOT"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
    }

    /// Add the host-owned runtime ABI to a normal adapter command. The
    /// development command remains available for legacy local packages.
    pub fn command_with_runtime(&self, cwd: &Path, runtime: &RuntimeEnvironment) -> Command {
        let mut command = self.command(cwd);
        for (key, value) in runtime.variables() {
            command.env(key, value);
        }
        command
    }
}

pub fn install(root: &Path, source: &Path) -> Result<PathBuf> {
    let package = Package::read(source)?;
    fs::create_dir_all(root)?;
    let root = root.canonicalize()?;
    ensure!(
        !root.starts_with(&package.root),
        "installation directory must not be inside source package"
    );
    let namespace = root.join(&package.manifest.id);
    if !namespace.exists() {
        fs::create_dir(&namespace)?;
    }
    ensure!(
        namespace.symlink_metadata()?.file_type().is_dir(),
        "plugin namespace is not a normal directory"
    );
    let dest = namespace.join(&package.manifest.version);
    ensure!(
        dest.symlink_metadata().is_err(),
        "this plugin version is already installed"
    );
    let staging = tempfile::Builder::new()
        .prefix(".stage-")
        .tempdir_in(namespace)?;
    copy_dir(&package.root, staging.path())?;
    Package::read(staging.path())?;
    fs::rename(staging.path(), &dest)?;
    Ok(dest)
}

pub fn copy_dir(source: &Path, dest: &Path) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let to = dest.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            fs::create_dir(&to)?;
            copy_dir(&entry.path(), &to)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), to)?;
        } else {
            anyhow::bail!("package may not contain symlinks or special files");
        }
    }
    Ok(())
}

pub fn installed(root: &Path) -> Result<Vec<Package>> {
    let (packages, issues) = installed_report(root)?;
    if let Some(issue) = issues.first() {
        anyhow::bail!(
            "invalid installed package {}: {}",
            issue.root.display(),
            issue.error
        );
    }
    Ok(packages)
}

pub fn installed_report(root: &Path) -> Result<(Vec<Package>, Vec<PackageIssue>)> {
    if !root.exists() {
        return Ok((vec![], vec![]));
    }
    let mut packages = Vec::new();
    let mut issues = Vec::new();
    for namespace in fs::read_dir(root)? {
        let namespace = namespace?;
        if !namespace.file_type()?.is_dir() {
            continue;
        }
        for version in fs::read_dir(namespace.path())? {
            let version = version?;
            if version.file_name() == "generations" {
                continue;
            }
            if !version.file_type()?.is_dir()
                || version.file_name().to_string_lossy().starts_with('.')
            {
                continue;
            }
            match Package::read(&version.path()).and_then(|package| {
                ensure!(
                    namespace.file_name() == package.manifest.id.as_str()
                        && version.file_name() == package.manifest.version.as_str(),
                    "installed package identity does not match its directory"
                );
                Ok(package)
            }) {
                Ok(package) => packages.push(package),
                Err(error) => issues.push(PackageIssue {
                    root: version.path(),
                    error: error.to_string(),
                }),
            }
        }
    }
    packages.sort_by(|a, b| {
        (&a.manifest.id, &a.manifest.version).cmp(&(&b.manifest.id, &b.manifest.version))
    });
    Ok((packages, issues))
}
