//! Bash command execution tool.
//!
//! This module provides the [`BashTool`] which allows the agent to execute
//! bash commands. It supports both full shell access and a restricted
//! "safe mode" that only allows predefined read-only commands.
//!
//! # Safety Features
//!
//! - **Safe mode**: Optional whitelist of allowed commands (ls, cat, grep, etc.)
//! - **Command chaining prevention**: Blocks `;`, `&&`, `||`, `|`, `&`, and
//!   newlines (all valid bash separators) in safe mode
//! - **Redirection prevention**: Blocks any `<`/`>` usage, including
//!   space-less forms like `2>` and `&>`
//! - **Working directory restriction**: Commands run in a specified directory
//! - **Timeout**: Configurable timeout prevents hanging commands; the whole
//!   process group is killed so detached grandchildren do not survive
//! - **Output cap**: Captured stdout/stderr is capped to prevent unbounded
//!   memory growth from commands like `cat /dev/urandom`
//!
//! # Tool Definition
//!
//! - **Name**: `bash`
//! - **Parameters**:
//!   - `command` (required): The bash command to execute
//!   - `timeout` (optional): Timeout in milliseconds (default: 120000)
//!   - `workdir` (optional): Working directory for the command

use anyhow::{Context, Result};
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::path::PathBuf;

/// Maximum bytes captured per output stream (stdout, stderr each).
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct BashArgs {
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    pub workdir: Option<String>,
}

fn default_timeout() -> u64 {
    120000
}

pub struct BashTool {
    pub working_directory: PathBuf,
    pub safe_mode: bool,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        include_str!("prompts/bash.txt")
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(BashArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: BashArgs = serde_json::from_value(args).map_err(|e| anyhow::anyhow!(e))?;

        if self.safe_mode {
            let trimmed = input.command.trim();
            let first_word = trimmed.split_whitespace().next().unwrap_or("");
            let allowed = [
                "ls", "cat", "grep", "rg", "find", "pwd", "echo", "wc", "head", "tail", "whoami",
                "date", "git", "printf", "sort", "uniq", "diff",
            ];

            if !allowed.contains(&first_word) {
                return Ok(ToolResult::new(
                    "bash",
                    format!(
                        "Command '{}' is not allowed in safe mode. Allowed commands: {:?}",
                        first_word, allowed
                    ),
                ));
            }

            // Newlines are valid bash separators: without this check,
            // `echo hi\nrm -rf ~/data` would pass the first-word allowlist
            // and execute both lines.
            if trimmed.contains('\n') || trimmed.contains('\r') {
                return Ok(ToolResult::new(
                    "bash",
                    "Complex command chaining (;, &&, ||, |, &, newlines) is not allowed in safe mode."
                        .to_string(),
                ));
            }

            // `|` and any `&` are also separators/pipelining constructs; a
            // pipe lets a whitelisted read-only command feed an arbitrary
            // second command (e.g. `cat x | tee /etc/cron.d/evil`).
            if trimmed.contains(';') || trimmed.contains('|') || trimmed.contains('&') {
                return Ok(ToolResult::new(
                    "bash",
                    "Complex command chaining (;, &&, ||, |, &, newlines) is not allowed in safe mode."
                        .to_string(),
                ));
            }

            // Any redirection character, including space-less forms
            // (`echo pwned>/tmp/x`, `2>`, `&>`) and heredocs/herestrings.
            if trimmed.contains('>') || trimmed.contains('<') {
                return Ok(ToolResult::new(
                    "bash",
                    "Output redirection (>, >>, <<) is not allowed in safe mode.".to_string(),
                ));
            }

            if trimmed.contains("$(") || trimmed.contains('`') {
                return Ok(ToolResult::new(
                    "bash",
                    "Command substitution ($(...), backticks) is not allowed in safe mode."
                        .to_string(),
                ));
            }

            // `find -exec`/`-ok` run arbitrary commands on every matched
            // file, turning the read-only allowlist into a write/delete
            // primitive (`find . -name '*' -exec rm -rf {} +`).
            if trimmed
                .split_whitespace()
                .any(|t| matches!(t, "-exec" | "-execdir" | "-ok" | "-okdir"))
            {
                return Ok(ToolResult::new(
                    "bash",
                    "Command execution via find (-exec/-ok) is not allowed in safe mode."
                        .to_string(),
                ));
            }
        }

        let work_dir = if let Some(wd) = &input.workdir {
            crate::fs_utils::resolve_read_path(&self.working_directory, wd, ctx.enforcement)
                .await
                .map_err(|e| anyhow::anyhow!(e))?
        } else {
            self.working_directory.clone()
        };

        let mut child = tokio::process::Command::new("bash");
        child
            .arg("-c")
            .arg(&input.command)
            .current_dir(&work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Own process group so the timeout path can kill detached
        // grandchildren (`bash -c "sleep 100 & wait"`), not just the direct
        // `bash -c` child.
        #[cfg(unix)]
        child.process_group(0);

        let mut child_process = child
            .spawn()
            .context("Failed to spawn bash command")
            .map_err(|e| anyhow::anyhow!(e))?;

        // Read both pipes concurrently with a cap, so a command streaming
        // gigabytes (`yes`, `cat /dev/urandom`) cannot exhaust memory before
        // the timeout fires.
        let stdout_pipe = child_process.stdout.take();
        let stderr_pipe = child_process.stderr.take();
        let stdout_reader =
            tokio::spawn(async move { read_capped(stdout_pipe, MAX_CAPTURE_BYTES).await });
        let stderr_reader =
            tokio::spawn(async move { read_capped(stderr_pipe, MAX_CAPTURE_BYTES).await });

        // Wait for the child, the timeout, or the run's abort flag. The abort
        // flag is how a cancelled run reaches a tool: without this, a command
        // keeps writing into the workspace after the UI already reported the
        // run as stopped.
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(input.timeout);
        let stop = loop {
            tokio::select! {
                biased;
                waited = child_process.wait() => break BashStop::Exited(waited),
                _ = tokio::time::sleep_until(deadline) => break BashStop::TimedOut,
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                    if ctx.is_aborted() {
                        break BashStop::Cancelled;
                    }
                }
            }
        };

        // Anything still running must die before the pipes are drained to EOF:
        // a live child holds the pipe open, so reading first would block until
        // the command exits on its own.
        if !matches!(&stop, BashStop::Exited(_)) {
            kill_process_tree(&mut child_process);
            let _ = child_process.wait().await;
        }

        let (captured_stdout, stdout_truncated) =
            stdout_reader.await.unwrap_or((Vec::new(), false));
        let (captured_stderr, stderr_truncated) =
            stderr_reader.await.unwrap_or((Vec::new(), false));

        let format_captured = |stdout: &str, stderr: &str| -> String {
            let mut result = String::new();
            if !stdout.is_empty() {
                result.push_str(stdout);
            }
            if !stderr.is_empty() {
                if !result.is_empty() {
                    result.push_str("\n--- stderr ---\n");
                }
                result.push_str(stderr);
            }
            result
        };

        match stop {
            BashStop::Exited(Ok(status)) => {
                let stdout = String::from_utf8_lossy(&captured_stdout);
                let stderr = String::from_utf8_lossy(&captured_stderr);

                if !status.success() {
                    let mut result = format_captured(&stdout, &stderr);
                    result.push_str(&format!("\n--- exit status: {} ---\n", status));
                    if stdout_truncated || stderr_truncated {
                        result.push_str("\n--- output truncated (exceeded capture cap) ---\n");
                    }
                    return Err(anyhow::anyhow!("Command failed: {}", result).into());
                }

                let mut result = format_captured(&stdout, &stderr);
                if stdout_truncated || stderr_truncated {
                    result.push_str("\n--- output truncated (exceeded capture cap) ---\n");
                }
                Ok(ToolResult::new("bash", result))
            }
            BashStop::Exited(Err(e)) => {
                Err(anyhow::anyhow!("Failed to wait for child process: {}", e).into())
            }
            interrupted => {
                // The child was killed above; report whatever output was
                // captured before the timeout or cancellation.
                let stdout = String::from_utf8_lossy(&captured_stdout);
                let stderr = String::from_utf8_lossy(&captured_stderr);
                let captured = format_captured(&stdout, &stderr);
                let mut result = match interrupted {
                    BashStop::Cancelled => "Command cancelled.".to_string(),
                    _ => format!("Command timed out after {} ms.", input.timeout),
                };
                if !captured.is_empty() {
                    result.push_str("\n--- partial output before it was stopped ---\n");
                    result.push_str(&captured);
                }
                if stdout_truncated || stderr_truncated {
                    result.push_str("\n--- output truncated (exceeded capture cap) ---\n");
                }
                Ok(ToolResult::new("bash", result))
            }
        }
    }
}

/// Why the tool stopped waiting for the child.
enum BashStop {
    Exited(std::io::Result<std::process::ExitStatus>),
    TimedOut,
    Cancelled,
}

/// Reads a pipe to EOF, keeping at most `cap` bytes and flagging truncation.
async fn read_capped(
    pipe: Option<impl tokio::io::AsyncRead + Unpin>,
    cap: usize,
) -> (Vec<u8>, bool) {
    use tokio::io::AsyncReadExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;
    let Some(mut pipe) = pipe else {
        return (buf, truncated);
    };
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = n.min(cap - buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (buf, truncated)
}

/// Kills the child and, on Unix, its whole process group.
fn kill_process_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // The child was spawned with process_group(0), so it leads its
            // own group; a negative pid signals every member.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
    let _ = child.start_kill();
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use tempfile::tempdir;
    use tokio::fs;

    #[tokio::test]
    async fn test_bash_tool_echo() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();
        let tool = BashTool {
            working_directory: working_dir,
            safe_mode: false,
        };
        let args = serde_json::json!({
            "command": "echo Hello world"
        });

        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert_eq!(result.output.trim(), "Hello world");
        Ok(())
    }

    #[tokio::test]
    async fn test_bash_tool_timeout() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();
        let tool = BashTool {
            working_directory: working_dir,
            safe_mode: false,
        };
        let args = serde_json::json!({
            "command": "sleep 2; echo done",
            "timeout": 500
        });

        let ctx = ToolContext::new("test-session", "test-msg");
        let started = std::time::Instant::now();
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("Command timed out"));
        // The pipes must be drained only after the child is killed; otherwise
        // the read blocks until the command exits on its own (2 s here).
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "the timeout waited for the command instead of killing it"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_bash_tool_file_create() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();
        let tool = BashTool {
            working_directory: working_dir.clone(),
            safe_mode: false,
        };

        let args = serde_json::json!({
            "command": "echo 'Bash content' > test_bash.txt"
        });

        let ctx = ToolContext::new("test-session", "test-msg");
        let _ = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let content = fs::read_to_string(working_dir.join("test_bash.txt")).await?;
        assert_eq!(content.trim(), "Bash content");
        Ok(())
    }

    #[tokio::test]
    async fn test_bash_tool_safe_mode() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();
        let tool = BashTool {
            working_directory: working_dir,
            safe_mode: true,
        };

        let args = serde_json::json!({
            "command": "echo hello"
        });
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert_eq!(result.output.trim(), "hello");

        let args = serde_json::json!({
            "command": "rm -rf /"
        });
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("not allowed in safe mode"));

        let args = serde_json::json!({
            "command": "echo hello > /tmp/out.txt"
        });
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("Output redirection"));

        let args = serde_json::json!({
            "command": "echo hello >> /tmp/out.txt"
        });
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("Output redirection"));

        let args = serde_json::json!({
            "command": "cat <<EOF\nhello\nEOF"
        });
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        // Rejected via the newline check (heredoc bodies span lines).
        assert!(result.output.contains("not allowed in safe mode"));

        let args = serde_json::json!({
            "command": "echo $(whoami)"
        });
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("Command substitution"));

        let args = serde_json::json!({
            "command": "echo `whoami`"
        });
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("Command substitution"));

        // --- bypass regressions -------------------------------------------------

        // Newlines are valid bash separators.
        let args = serde_json::json!({
            "command": "echo hi\nrm -rf ./data"
        });
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("not allowed in safe mode"));

        // Space-less redirection.
        let args = serde_json::json!({
            "command": "echo pwned>/tmp/gewu_safe_mode_test"
        });
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("Output redirection"));

        // fd redirection (`2>`).
        let args = serde_json::json!({
            "command": "ls 2>/tmp/gewu_safe_mode_test"
        });
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("Output redirection"));

        // Pipes feed non-whitelisted second commands.
        let args = serde_json::json!({
            "command": "cat /etc/hostname | tee /tmp/gewu_safe_mode_test"
        });
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("not allowed in safe mode"));

        // Backgrounding runs a second command asynchronously.
        let args = serde_json::json!({
            "command": "echo hi & rm -rf ./data"
        });
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("not allowed in safe mode"));

        // find -exec turns read-only tools into a write/delete primitive.
        let args = serde_json::json!({
            "command": "find . -name '*' -exec rm -rf {} +"
        });
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("not allowed in safe mode"));

        Ok(())
    }

    #[tokio::test]
    async fn test_bash_tool_timeout_preserves_partial_output() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();
        let tool = BashTool {
            working_directory: working_dir,
            safe_mode: false,
        };

        let args = serde_json::json!({
            "command": "echo before-sleep; sleep 5",
            "timeout": 400
        });
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("Command timed out"));
        assert!(
            result.output.contains("before-sleep"),
            "partial output captured before the timeout should be preserved"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_bash_tool_output_is_capped() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();
        let tool = BashTool {
            working_directory: working_dir,
            safe_mode: false,
        };

        // 3 MB of output against a 1 MB capture cap must not be buffered
        // in full.
        let args = serde_json::json!({
            "command": "head -c 3145728 /dev/zero | tr '\\0' 'x'",
            "timeout": 30000
        });
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("output truncated"));
        assert!(result.output.len() < 2 * 1024 * 1024);
        Ok(())
    }
}
