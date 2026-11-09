//! Concrete, disposable shell execution in a Singularity image.
//!
//! This is not a tool, a service or a persistent sandbox. It is the one place
//! that knows how to start a container for one command and return a structured
//! result. Callers own the workspace lifetime and decide what to do with files.
//!
//! Guarantees this module is responsible for:
//!
//! * the command runs with a new user namespace, PID namespace, minimal
//!   filesystem, no network and a clean environment;
//! * only the mounts listed in [`ShellEnvironment`] are visible;
//! * stdout/stderr are drained with a bound and never block the child;
//! * timeout and cancellation kill the whole process group and reap it.
//!
//! It does not claim a cgroup memory cap. `--contain` is not `--containall`;
//! the build host here rejects `--containall`.

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use gewu_core::CancelHandle;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};

/// One bind mount. `container` must be an absolute path inside the image.
#[derive(Debug, Clone)]
pub struct ShellMount {
    pub host: PathBuf,
    pub container: PathBuf,
    pub read_only: bool,
}

/// Hard limits for one command.
#[derive(Debug, Clone)]
pub struct ShellLimits {
    pub wall_timeout: Duration,
    /// Bytes kept for the model. Extra bytes are drained and counted, not kept.
    pub max_output_bytes: usize,
}

impl Default for ShellLimits {
    fn default() -> Self {
        Self {
            wall_timeout: Duration::from_secs(120),
            max_output_bytes: 64 * 1024,
        }
    }
}

/// Everything one command needs. Built by the application, never by the model.
#[derive(Debug, Clone)]
pub struct ShellEnvironment {
    pub image: PathBuf,
    pub workspace: PathBuf,
    pub outputs: PathBuf,
    pub inputs: PathBuf,
    pub tmp: PathBuf,
    pub extra_mounts: Vec<ShellMount>,
    pub limits: ShellLimits,
    /// Container path of the Singularity binary. Overridable for tests.
    pub singularity: String,
    /// Extra container environment entries, passed explicitly.
    pub env: Vec<(String, String)>,
}

impl ShellEnvironment {
    pub fn new(
        image: impl Into<PathBuf>,
        workspace: impl Into<PathBuf>,
        outputs: impl Into<PathBuf>,
        inputs: impl Into<PathBuf>,
        tmp: impl Into<PathBuf>,
    ) -> Self {
        Self {
            image: image.into(),
            workspace: workspace.into(),
            outputs: outputs.into(),
            inputs: inputs.into(),
            tmp: tmp.into(),
            extra_mounts: Vec::new(),
            limits: ShellLimits::default(),
            singularity: std::env::var("TACHYON_SINGULARITY")
                .unwrap_or_else(|_| "singularity".into()),
            env: Vec::new(),
        }
    }

    pub fn with_limit(mut self, timeout: Duration, max_output_bytes: usize) -> Self {
        self.limits = ShellLimits {
            wall_timeout: timeout,
            max_output_bytes,
        };
        self
    }

    pub fn with_mount(mut self, mount: ShellMount) -> Self {
        self.extra_mounts.push(mount);
        self
    }

    /// Add one container environment entry. Passed with `--env` because
    /// `--cleanenv` suppresses the host environment.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Check the inputs before spawning anything. A failure here is a
    /// configuration error, not a command failure.
    pub fn validate(&self) -> Result<(), ShellError> {
        if !self.image.is_file() {
            return Err(ShellError::Invalid(format!(
                "analysis image {} does not exist",
                self.image.display()
            )));
        }
        for (label, path) in [
            ("workspace", &self.workspace),
            ("outputs", &self.outputs),
            ("inputs", &self.inputs),
            ("tmp", &self.tmp),
        ] {
            if !path.is_dir() {
                return Err(ShellError::Invalid(format!(
                    "{label} {} is not a directory",
                    path.display()
                )));
            }
            let metadata = std::fs::metadata(path).map_err(|error| {
                ShellError::Invalid(format!("{label} {}: {error}", path.display()))
            })?;
            if !metadata.is_dir() {
                return Err(ShellError::Invalid(format!(
                    "{label} {} is not a directory",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    fn bind_args(&self, args: &mut Vec<String>) {
        let mount = |args: &mut Vec<String>, host: &Path, container: &str, read_only: bool| {
            let mut spec = format!("{}:{container}", host.display());
            if read_only {
                spec.push_str(":ro");
            }
            args.push("--bind".into());
            args.push(spec);
        };
        mount(args, &self.workspace, "/workspace", false);
        mount(args, &self.outputs, "/outputs", false);
        mount(args, &self.inputs, "/inputs", true);
        mount(args, &self.tmp, "/tmp", false);
        for extra in &self.extra_mounts {
            mount(
                args,
                &extra.host,
                extra.container.to_str().unwrap_or("/"),
                extra.read_only,
            );
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("invalid shell environment: {0}")]
    Invalid(String),
    #[error("cannot start the analysis command: {0}")]
    Spawn(String),
    #[error("cannot read command output: {0}")]
    Io(String),
}

/// The structured result of one command. No artifact and no analysis judgement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellOutcome {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub duration_ms: u64,
    pub stdout: String,
    pub stderr: String,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub output_truncated: bool,
}

impl ShellOutcome {
    pub fn succeeded(&self) -> bool {
        !self.timed_out && !self.cancelled && self.exit_code == Some(0)
    }
}

/// Run one command in the environment's image.
///
/// `workdir` is a container path; it must be one of `/workspace`, `/outputs` or
/// `/tmp` (or an extra mount's container path) so a command cannot request a
/// host path as its working directory.
pub async fn execute(
    environment: &ShellEnvironment,
    command: &str,
    workdir: &str,
    cancel: Option<&CancelHandle>,
) -> Result<ShellOutcome, ShellError> {
    environment.validate()?;
    let allowed = ["/workspace", "/outputs", "/tmp"];
    if !workdir.starts_with('/')
        || !(allowed
            .iter()
            .any(|prefix| workdir == *prefix || workdir.starts_with(&format!("{prefix}/")))
            || environment
                .extra_mounts
                .iter()
                .any(|mount| mount.container.to_str() == Some(workdir)))
    {
        return Err(ShellError::Invalid(format!(
            "workdir {workdir:?} is not one of /workspace, /outputs, /tmp or a declared mount"
        )));
    }
    if command.len() > 64 * 1024 {
        return Err(ShellError::Invalid("command exceeds 64 KiB".into()));
    }

    let mut args: Vec<String> = vec![
        "exec".into(),
        "--userns".into(),
        "--contain".into(),
        "--net".into(),
        "--no-home".into(),
        "--cleanenv".into(),
        "--pid".into(),
        "--no-mount".into(),
        "hostfs,cwd,tmp".into(),
        "--network".into(),
        "none".into(),
    ];
    environment.bind_args(&mut args);
    for (key, value) in &environment.env {
        args.push("--env".into());
        args.push(format!("{key}={value}"));
    }
    args.push("--pwd".into());
    args.push(workdir.to_string());
    args.push(environment.image.display().to_string());
    args.push("/bin/bash".into());
    args.push("-c".into());
    args.push(command.to_string());

    let mut process = tokio::process::Command::new(&environment.singularity);
    process
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Own process group so timeout/cancel can kill background children too.
    unsafe {
        process.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let started = Instant::now();
    let mut child = process
        .spawn()
        .map_err(|error| ShellError::Spawn(format!("{}: {error}", environment.singularity)))?;
    let pid = child.id().map(|pid| pid as i32);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ShellError::Spawn("stdout pipe missing".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ShellError::Spawn("stderr pipe missing".into()))?;
    let limit = environment.limits.max_output_bytes;
    let stdout_task = tokio::spawn(read_bounded(stdout, limit));
    let stderr_task = tokio::spawn(read_bounded(stderr, limit));

    let mut timed_out = false;
    let mut cancelled = false;
    let deadline = tokio::time::Instant::now() + environment.limits.wall_timeout;
    let status = loop {
        tokio::select! {
            waited = child.wait() => break waited.map_err(|error| ShellError::Io(error.to_string()))?,
            _ = tokio::time::sleep_until(deadline) => {
                timed_out = true;
                kill_group(pid, &mut child).await;
                break child.wait().await.map_err(|error| ShellError::Io(error.to_string()))?;
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if cancel.is_some_and(|handle| handle.is_cancelled()) {
                    cancelled = true;
                    kill_group(pid, &mut child).await;
                    break child.wait().await.map_err(|error| ShellError::Io(error.to_string()))?;
                }
            }
        }
    };

    // A background child that escaped the group could hold the pipes open.
    // Wait briefly, then stop reading rather than hanging the tool.
    let (stdout, stdout_bytes, stdout_truncated) = finish_reader(stdout_task).await;
    let (stderr, stderr_bytes, stderr_truncated) = finish_reader(stderr_task).await;

    let signal = status.signal();
    Ok(ShellOutcome {
        exit_code: status.code(),
        signal,
        timed_out,
        cancelled,
        duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        stdout,
        stderr,
        stdout_bytes,
        stderr_bytes,
        output_truncated: stdout_truncated || stderr_truncated,
    })
}

async fn read_bounded<R: AsyncRead + Unpin>(mut reader: R, limit: usize) -> (String, u64, bool) {
    let mut buffer = [0u8; 8192];
    let mut kept = Vec::new();
    let mut total = 0u64;
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                total += read as u64;
                if kept.len() < limit {
                    let take = read.min(limit - kept.len());
                    kept.extend_from_slice(&buffer[..take]);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    (
        String::from_utf8_lossy(&kept).to_string(),
        total,
        total > limit as u64,
    )
}

async fn finish_reader(task: tokio::task::JoinHandle<(String, u64, bool)>) -> (String, u64, bool) {
    match tokio::time::timeout(Duration::from_secs(2), task).await {
        Ok(Ok(value)) => value,
        Ok(Err(_)) => (String::new(), 0, false),
        Err(_) => (String::new(), 0, true),
    }
}

async fn kill_group(pid: Option<i32>, child: &mut tokio::process::Child) {
    if let Some(pid) = pid {
        // Negative pid targets the process group created by setsid().
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
}
