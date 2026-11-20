//! Runtime acquisition providers.
//!
//! A provider turns a typed [`RuntimeProvider`] into the same packed-runtime
//! shape the host already knows: a directory with `runtime.json`, a launcher,
//! and a probe. Managed and external runtimes therefore reuse the existing
//! `RuntimeBundle` validation, probe, and activation path.

use gewu_protocol::release::{ProbeSpec, RuntimeProvider, Target};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fmt,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{process::Command, time::timeout};

#[derive(Debug)]
pub enum ProviderError {    Unavailable(String),
    Invalid(String),
    ProbeFailed(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) => write!(f, "runtime provider is unavailable: {message}"),
            Self::Invalid(message) => write!(f, "runtime provider is invalid: {message}"),
            Self::ProbeFailed(message) => write!(f, "runtime provider probe failed: {message}"),
        }
    }
}

impl std::error::Error for ProviderError {}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub provider: String,
    pub kind: String,
    pub executable: Option<String>,
    pub available: bool,
    pub detail: String,
}

/// Search-path lookup without a shell. Absolute or relative commands that
/// contain a separator are checked directly.
pub fn find_command(command: &str) -> Option<PathBuf> {
    find_command_in(command, std::env::var_os("PATH"), dirs::home_dir())
}

fn find_command_in(
    command: &str,
    path: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    if command.is_empty() || command.contains('\0') {
        return None;
    }
    if command.contains('/') {
        let path = PathBuf::from(command);
        return is_executable(&path).then_some(path);
    }
    let mut directories = path
        .as_deref()
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if let Some(home) = home {
        for directory in [home.join(".local/bin"), home.join("bin"), home.join("micromamba/bin")] {
            if !directories.contains(&directory) {
                directories.push(directory);
            }
        }
    }
    directories
        .into_iter()
        .map(|dir| dir.join(command))
        .find(|candidate| is_executable(candidate))
}

pub fn is_executable(path: &Path) -> bool {    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Whether a managed provider's toolchain is present on this host.
pub fn managed_available(manager: &str) -> bool {
    std::env::var_os("GEWU_MICROMAMBA")
        .map(PathBuf::from)
        .filter(|path| is_executable(path))
        .or_else(|| find_command(manager))
        .is_some()
}

pub fn managed_prefix_key(plugin: &str, provider: &RuntimeProvider) -> Result<String, ProviderError> {
    if !matches!(provider, RuntimeProvider::Managed { .. }) {
        return Err(ProviderError::Invalid("provider is not managed".into()));
    }
    let value = serde_json::json!({
        "plugin": plugin,
        "provider": provider,
        "target": provider.target(),
    });
    let bytes = serde_json::to_vec(&value)
        .map_err(|error| ProviderError::Invalid(error.to_string()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

pub fn managed_complete(prefix: &Path) -> bool {
    prefix.join(".gewu-complete").is_file() && is_executable(&prefix.join("bin/python3"))
}

/// Resolve an external provider's interpreter. An explicit `override_path`
/// (for example `--executable`) wins; then the provider's environment
/// variable; then its declared command names.
pub fn resolve_external(
    provider: &RuntimeProvider,
    override_path: Option<&Path>,
) -> Result<PathBuf, ProviderError> {
    let RuntimeProvider::External { executable, .. } = provider else {
        return Err(ProviderError::Invalid("provider is not external".into()));
    };
    if let Some(path) = override_path {
        return is_executable(path)
            .then(|| path.to_path_buf())
            .ok_or_else(|| ProviderError::Unavailable(format!("{} is not executable", path.display())));
    }
    if let Some(name) = executable.env.as_deref()
        && let Some(value) = std::env::var_os(name)
        && !value.is_empty()
    {
        let path = PathBuf::from(value);
        if is_executable(&path) {
            return Ok(path);
        }
        return Err(ProviderError::Unavailable(format!("${name} does not point at an executable")));
    }
    executable
        .commands
        .iter()
        .find_map(|command| find_command(command))
        .ok_or_else(|| ProviderError::Unavailable(format!("none of {:?} is on PATH", executable.commands)))
}

/// Run the provider's probe against an interpreter before synthesizing a root.
pub async fn probe_executable(executable: &Path, probe: &ProbeSpec) -> Result<(), ProviderError> {
    let deadline = Duration::from_secs(probe.timeout_seconds.unwrap_or(120));
    let mut command = Command::new(executable);
    command.args(&probe.args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).env_clear();
    for key in ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let output = timeout(deadline, command.output()).await.map_err(|_| ProviderError::ProbeFailed("probe timed out".into()))?
        .map_err(|error| ProviderError::ProbeFailed(error.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ProviderError::ProbeFailed(trim_message(&stderr)));
    }
    Ok(())
}

/// Materialize an external provider as a packed runtime root. The generated
/// `bin/*` wrappers are the ABI adapters already expect.
pub fn synthesize_external(
    root: &Path,
    plugin: &str,
    version: &str,
    target: &Target,
    provider: &RuntimeProvider,
    executable: &Path,
) -> Result<(), ProviderError> {
    let RuntimeProvider::External { probe, binaries, .. } = provider else {
        return Err(ProviderError::Invalid("provider is not external".into()));
    };
    let binary = executable
        .canonicalize()
        .map_err(|error| ProviderError::Invalid(format!("cannot resolve executable: {error}")))?;
    write_runtime_root(root, plugin, version, target, provider.id(), "bin/launch", "bin/probe")?;
    write_wrapper(root, "bin/launch", &binary, &[])?;
    write_wrapper(root, "bin/probe", &binary, &probe.args)?;
    write_binaries(root, &binary, binaries)?;
    default_python_binary(root, &binary)?;
    Ok(())
}

/// Materialize a managed environment as a packed runtime root.
pub fn synthesize_managed(
    root: &Path,
    plugin: &str,
    version: &str,
    target: &Target,
    provider: &RuntimeProvider,
    prefix: &Path,
) -> Result<(), ProviderError> {
    let RuntimeProvider::Managed { probe, binaries, .. } = provider else {
        return Err(ProviderError::Invalid("provider is not managed".into()));
    };
    let python = prefix.join("bin/python3");
    if !is_executable(&python) {
        return Err(ProviderError::Invalid("managed environment has no bin/python3".into()));
    }
    write_runtime_root(root, plugin, version, target, provider.id(), "bin/launch", "bin/probe")?;
    write_wrapper(root, "bin/launch", &python, &[])?;
    write_wrapper(root, "bin/probe", &python, &probe.args)?;
    write_binaries(root, &python, binaries)?;
    default_python_binary(root, &python)?;
    Ok(())
}

/// Adapters look for `$GEWU_RUNTIME_ROOT/bin/python3`; always provide it even
/// when a plan omits the `binaries` map.
fn default_python_binary(root: &Path, executable: &Path) -> Result<(), ProviderError> {
    if !root.join("bin/python3").exists() {
        write_wrapper(root, "bin/python3", executable, &[])?;
    }
    Ok(())
}

/// Create a micromamba prefix with the provider's conda packages and pip
/// requirements. The prefix is host-owned and never touches a user env.
pub async fn install_managed(
    provider: &RuntimeProvider,
    prefix: &Path,
    timeout_seconds: u64,
) -> Result<(), ProviderError> {
    let RuntimeProvider::Managed { manager, .. } = provider else {
        return Err(ProviderError::Invalid("provider is not managed".into()));
    };
    if manager != "micromamba" {
        return Err(ProviderError::Invalid(format!("unsupported manager {manager}")));
    }
    let binary = std::env::var_os("GEWU_MICROMAMBA")
        .map(PathBuf::from)
        .filter(|path| is_executable(path))
        .or_else(|| find_command("micromamba"))
        .ok_or_else(|| ProviderError::Unavailable("micromamba is not installed".into()))?;
    install_managed_with_binary(provider, prefix, timeout_seconds, &binary).await
}

async fn install_managed_with_binary(
    provider: &RuntimeProvider,
    prefix: &Path,
    timeout_seconds: u64,
    binary: &Path,
) -> Result<(), ProviderError> {
    let RuntimeProvider::Managed { manager, channels, packages, pip, pip_index, pip_extra_index, probe, .. } = provider else {
        return Err(ProviderError::Invalid("provider is not managed".into()));
    };
    if manager != "micromamba" {
        return Err(ProviderError::Invalid(format!("unsupported manager {manager}")));
    }
    let parent = prefix
        .parent()
        .ok_or_else(|| ProviderError::Invalid("managed prefix has no parent".into()))?;
    std::fs::create_dir_all(parent).map_err(|error| ProviderError::Invalid(error.to_string()))?;
    let _lock = ManagedLock::acquire(prefix, timeout_seconds).await?;
    if managed_complete(prefix) {
        return Ok(());
    }
    if prefix.exists() {
        std::fs::remove_dir_all(prefix)
            .map_err(|error| ProviderError::Invalid(format!("cannot remove incomplete managed prefix: {error}")))?;
    }
    let temporary = tempfile::Builder::new()
        .prefix(".managed-")
        .tempdir_in(parent)
        .map_err(|error| ProviderError::Invalid(error.to_string()))?;
    let work = temporary.path().join("prefix");
    std::fs::create_dir_all(&work).map_err(|error| ProviderError::Invalid(error.to_string()))?;
    let deadline = Duration::from_secs(timeout_seconds);
    let mut create = Command::new(binary);
    create.arg("create").arg("--yes").arg("--prefix").arg(&work);
    for channel in channels {
        create.arg("--channel").arg(channel);
    }
    create.args(packages);
    run_install(create, deadline, "micromamba create").await?;
    if !pip.is_empty() {
        let mut install = Command::new(binary);
        install.arg("run").arg("--prefix").arg(&work).arg("python").arg("-m").arg("pip").arg("install").arg("--disable-pip-version-check").arg("--prefer-binary");
        let index = std::env::var("GEWU_PIP_INDEX_URL").ok().or_else(|| pip_index.clone());
        let extra = std::env::var("GEWU_PIP_EXTRA_INDEX_URL").ok().or_else(|| pip_extra_index.clone());
        if let Some(index) = index.filter(|value| !value.trim().is_empty()) {
            install.arg("--index-url").arg(index);
        }
        if let Some(extra) = extra.filter(|value| !value.trim().is_empty()) {
            install.arg("--extra-index-url").arg(extra);
        }
        install.args(pip);
        run_install(install, deadline, "pip install").await?;
    }
    probe_managed(&work, probe, deadline).await?;
    std::fs::write(work.join(".gewu-complete"), b"gewu managed runtime\n")
        .map_err(|error| ProviderError::Invalid(error.to_string()))?;
    std::fs::rename(&work, prefix)
        .map_err(|error| ProviderError::Invalid(format!("cannot activate managed prefix: {error}")))?;
    Ok(())
}

async fn probe_managed(
    prefix: &Path,
    probe: &ProbeSpec,
    deadline: Duration,
) -> Result<(), ProviderError> {
    let python = prefix.join("bin/python3");
    if !is_executable(&python) {
        return Err(ProviderError::Invalid("managed environment has no bin/python3".into()));
    }
    let mut command = Command::new(&python);
    command.args(&probe.args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).env_clear();
    for key in ["HOME", "LANG", "LC_ALL", "TMPDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut path = prefix.join("bin").display().to_string();
    if let Some(value) = std::env::var_os("PATH") {
        path.push(':');
        path.push_str(&value.to_string_lossy());
    }
    command.env("PATH", path);
    let library = prefix.join("lib");
    if library.is_dir() {
        command.env("LD_LIBRARY_PATH", library);
    }
    let output = timeout(deadline, command.output())
        .await
        .map_err(|_| ProviderError::ProbeFailed("managed runtime probe timed out".into()))?
        .map_err(|error| ProviderError::ProbeFailed(error.to_string()))?;
    if !output.status.success() {
        return Err(ProviderError::ProbeFailed(trim_message(&String::from_utf8_lossy(&output.stderr))));
    }
    Ok(())
}

struct ManagedLock {
    path: PathBuf,
}

impl ManagedLock {
    async fn acquire(prefix: &Path, timeout_seconds: u64) -> Result<Self, ProviderError> {
        let path = PathBuf::from(format!("{}.lock", prefix.display()));
        let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
        loop {
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if Instant::now() >= deadline {
                        return Err(ProviderError::Unavailable(format!(
                            "managed runtime is locked: {}",
                            prefix.display()
                        )));
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => return Err(ProviderError::Invalid(error.to_string())),
            }
        }
    }
}

impl Drop for ManagedLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}

async fn run_install(mut command: Command, deadline: Duration, what: &str) -> Result<(), ProviderError> {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = timeout(deadline, command.output()).await.map_err(|_| ProviderError::ProbeFailed(format!("{what} timed out")))?
        .map_err(|error| ProviderError::Unavailable(error.to_string()))?;
    if !output.status.success() {
        return Err(ProviderError::ProbeFailed(format!("{what} failed: {}", trim_message(&String::from_utf8_lossy(&output.stderr)))));
    }
    Ok(())
}

fn write_runtime_root(
    root: &Path,
    plugin: &str,
    version: &str,
    target: &Target,
    id: &str,
    launcher: &str,
    probe: &str,
) -> Result<(), ProviderError> {
    std::fs::create_dir_all(root.join("bin")).map_err(|error| ProviderError::Invalid(error.to_string()))?;
    let manifest = serde_json::json!({
        "schemaVersion": "gewu.runtime/v1",
        "id": id,
        "version": version,
        "plugin": plugin,
        "target": target,
        "launcher": launcher,
        "probe": probe,
        "capabilities": ["external-runtime"],
    });
    std::fs::write(root.join("runtime.json"), serde_json::to_vec_pretty(&manifest).map_err(|error| ProviderError::Invalid(error.to_string()))?)
        .map_err(|error| ProviderError::Invalid(error.to_string()))
}

fn write_wrapper(root: &Path, relative: &str, executable: &Path, args: &[String]) -> Result<(), ProviderError> {
    let program = quote_shell(&executable.to_string_lossy())?;
    let mut script = String::from("#!/bin/sh\nset -eu\nexec ");
    script.push_str(&program);
    for arg in args {
        script.push(' ');
        script.push_str(&quote_shell(arg)?);
    }
    script.push_str(" \"$@\"\n");
    write_executable(&root.join(relative), &script)
}

fn write_binaries(root: &Path, executable: &Path, binaries: &std::collections::BTreeMap<String, String>) -> Result<(), ProviderError> {
    for (name, value) in binaries {
        if name.is_empty() || name.contains('/') || name.contains('\0') {
            return Err(ProviderError::Invalid(format!("invalid runtime binary name {name:?}")));
        }
        let program = if value == "{executable}" {
            executable.to_path_buf()
        } else {
            find_command(value).ok_or_else(|| ProviderError::Unavailable(format!("binary {value} is not on PATH")))?
        };
        write_wrapper(root, &format!("bin/{name}"), &program, &[])?;
    }
    Ok(())
}

fn write_executable(path: &Path, contents: &str) -> Result<(), ProviderError> {
    std::fs::write(path, contents).map_err(|error| ProviderError::Invalid(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|error| ProviderError::Invalid(error.to_string()))?;
    }
    Ok(())
}

fn quote_shell(value: &str) -> Result<String, ProviderError> {
    if value.contains('\0') || value.contains('\n') {
        return Err(ProviderError::Invalid("runtime path contains a forbidden character".into()));
    }
    Ok(format!("'{}'", value.replace('\'', r"'\''")))
}

fn trim_message(value: &str) -> String {
    let message = value.trim();
    let mut chars = message.chars();
    let clipped: String = chars.by_ref().take(400).collect();
    if chars.next().is_some() { format!("{clipped}…") } else { clipped }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_protocol::release::{ExecutableHint, RuntimeProvider};

    fn target() -> Target {
        Target { os: "linux".into(), arch: "x86_64".into(), libc: Some("glibc".into()) }
    }

    #[tokio::test]
    async fn synthesized_external_root_loads_and_probes() {
        let temp = tempfile::tempdir().unwrap();
        let provider = RuntimeProvider::External {
            id: "local-python".into(),
            target: target(),
            executable: ExecutableHint { env: None, commands: vec!["python3".into()] },
            probe: ProbeSpec { args: vec!["-c".into(), "print('ok')".into()], timeout_seconds: Some(10) },
            binaries: Default::default(),
        };
        let executable = find_command("python3").unwrap();
        synthesize_external(temp.path(), "demo", "1.0.0", &target(), &provider, &executable).unwrap();
        let bundle = crate::plugins::runtime::RuntimeBundle::load(temp.path(), "demo", &target()).unwrap();
        bundle.probe(temp.path(), temp.path(), Duration::from_secs(10)).await.unwrap();
    }

    #[test]
    fn missing_command_is_reported_as_unavailable() {
        let provider = RuntimeProvider::External {
            id: "missing".into(),
            target: target(),
            executable: ExecutableHint { env: None, commands: vec!["definitely-not-a-real-binary-xyz".into()] },
            probe: ProbeSpec { args: vec![], timeout_seconds: None },
            binaries: Default::default(),
        };
        assert!(matches!(resolve_external(&provider, None), Err(ProviderError::Unavailable(_))));
    }

    #[test]
    fn command_discovery_checks_the_user_local_bin_directory() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("micromamba");
        write_executable(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(
            find_command_in("micromamba", Some(std::ffi::OsString::new()), Some(temp.path().to_path_buf())),
            Some(executable)
        );
    }

    #[test]
    fn managed_prefix_key_changes_when_provider_inputs_change() {
        let first = RuntimeProvider::Managed {
            id: "demo".into(),
            target: target(),
            manager: "micromamba".into(),
            channels: vec!["conda-forge".into()],
            packages: vec!["python=3.11".into()],
            pip: vec![],
            pip_index: None,
            pip_extra_index: None,
            probe: ProbeSpec { args: vec![], timeout_seconds: None },
            binaries: Default::default(),
        };
        let mut second = first.clone();
        if let RuntimeProvider::Managed { packages, .. } = &mut second {
            packages.push("cairo".into());
        }
        assert_ne!(
            managed_prefix_key("demo", &first).unwrap(),
            managed_prefix_key("demo", &second).unwrap()
        );
    }

    #[tokio::test]
    async fn failed_managed_install_does_not_leave_a_final_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let manager = temp.path().join("micromamba");
        write_executable(
            &manager,
            "#!/bin/sh\nset -eu\nif [ \"$1\" = create ]; then\n  prefix=\"\"\n  while [ \"$#\" -gt 0 ]; do\n    if [ \"$1\" = --prefix ]; then prefix=$2; shift 2; else shift; fi\n  done\n  mkdir -p \"$prefix/bin\"\nelif [ \"$1\" = run ]; then\n  exit 7\nfi\n",
        )
        .unwrap();
        let provider = RuntimeProvider::Managed {
            id: "demo".into(),
            target: target(),
            manager: "micromamba".into(),
            channels: vec![],
            packages: vec!["python=3.11".into()],
            pip: vec!["broken==1.0".into()],
            pip_index: None,
            pip_extra_index: None,
            probe: ProbeSpec { args: vec![], timeout_seconds: Some(1) },
            binaries: Default::default(),
        };
        let prefix = temp.path().join("runtime");
        assert!(install_managed_with_binary(&provider, &prefix, 2, &manager).await.is_err());
        assert!(!prefix.exists());
        assert!(!temp.path().join("runtime.lock").exists());
    }
}
