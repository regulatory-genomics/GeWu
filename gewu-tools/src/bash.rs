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
//! - **Cancellation**: Canceling the surrounding run kills the command's
//!   process group instead of waiting for it to finish
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

fn safe_mode_error(command: &str) -> Option<String> {
    let trimmed = command.trim();
    let first_word = trimmed.split_whitespace().next().unwrap_or("");
    let allowed = [
        "ls", "cat", "grep", "rg", "find", "pwd", "echo", "wc", "head", "tail", "whoami", "date",
        "git", "printf", "sort", "uniq", "diff",
    ];

    if !allowed.contains(&first_word) {
        return Some(format!(
            "Command '{}' is not allowed in safe mode. Allowed commands: {:?}",
            first_word, allowed
        ));
    }
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return Some(
            "Complex command chaining (;, &&, ||, |, &, newlines) is not allowed in safe mode."
                .to_string(),
        );
    }
    if trimmed.contains(';') || trimmed.contains('|') || trimmed.contains('&') {
        return Some(
            "Complex command chaining (;, &&, ||, |, &, newlines) is not allowed in safe mode."
                .to_string(),
        );
    }
    if trimmed.contains('>') || trimmed.contains('<') {
        return Some("Output redirection (>, >>, <<) is not allowed in safe mode.".to_string());
    }
    if trimmed.contains("$(") || trimmed.contains('`') {
        return Some(
            "Command substitution ($(...), backticks) is not allowed in safe mode.".to_string(),
        );
    }
    if trimmed
        .split_whitespace()
        .any(|t| matches!(t, "-exec" | "-execdir" | "-ok" | "-okdir"))
    {
        return Some(
            "Command execution via find (-exec/-ok) is not allowed in safe mode.".to_string(),
        );
    }
    None
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

        if self.safe_mode
            && let Some(error) = safe_mode_error(&input.command)
        {
            return Ok(ToolResult::new("bash", error));
        }

        // The turn may already be canceled when the call is dispatched;
        // spawning a process only to kill it moments later is pointless.
        if ctx.is_aborted() {
            return Ok(ToolResult::new(
                "bash",
                "Command aborted because the surrounding run was canceled.",
            ));
        }

        let working_directory = ctx.active_working_directory(&self.working_directory);
        let work_dir = if let Some(wd) = &input.workdir {
            crate::fs_utils::resolve_read_path(working_directory, wd, ctx.enforcement)
                .await
                .map_err(|e| anyhow::anyhow!(e))?
        } else {
            working_directory.to_path_buf()
        };

        let mut child = tokio::process::Command::new("bash");
        child
            .arg("-c")
            .arg(&input.command)
            .current_dir(&work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        crate::process::own_group(&mut child);

        let mut child_process = child
            .spawn()
            .context("Failed to spawn bash command")
            .map_err(|e| anyhow::anyhow!(e))?;

        // Read both pipes concurrently with a cap, so a command streaming
        // gigabytes (`yes`, `cat /dev/urandom`) cannot exhaust memory before
        // the timeout fires.
        let stdout_pipe = child_process.stdout.take();
        let stderr_pipe = child_process.stderr.take();
        let mut stdout_reader =
            tokio::spawn(async move { read_capped(stdout_pipe, MAX_CAPTURE_BYTES).await });
        let mut stderr_reader =
            tokio::spawn(async move { read_capped(stderr_pipe, MAX_CAPTURE_BYTES).await });

        let wait_result = tokio::select! {
            biased;
            result = tokio::time::timeout(
                std::time::Duration::from_millis(input.timeout),
                child_process.wait(),
            ) => match result {
                Ok(result) => WaitOutcome::Done(result),
                Err(_) => WaitOutcome::TimedOut,
            },
            _ = wait_for_abort(ctx) => WaitOutcome::Aborted,
        };

        if !matches!(wait_result, WaitOutcome::Done(Ok(_))) {
            crate::process::kill_group(&mut child_process);
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(1), child_process.wait()).await;
        }

        let drain_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let (captured_stdout, stdout_truncated) =
            match tokio::time::timeout_at(drain_deadline, &mut stdout_reader).await {
                Ok(result) => result.unwrap_or((Vec::new(), false)),
                Err(_) => {
                    stdout_reader.abort();
                    (Vec::new(), true)
                }
            };
        let (captured_stderr, stderr_truncated) =
            match tokio::time::timeout_at(drain_deadline, &mut stderr_reader).await {
                Ok(result) => result.unwrap_or((Vec::new(), false)),
                Err(_) => {
                    stderr_reader.abort();
                    (Vec::new(), true)
                }
            };

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

        match wait_result {
            WaitOutcome::Done(Ok(status)) => {
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
            WaitOutcome::Done(Err(e)) => {
                Err(anyhow::anyhow!("Failed to wait for child process: {}", e).into())
            }
            WaitOutcome::TimedOut => {
                let stdout = String::from_utf8_lossy(&captured_stdout);
                let stderr = String::from_utf8_lossy(&captured_stderr);
                let captured = format_captured(&stdout, &stderr);
                let mut result = format!("Command timed out after {} ms.", input.timeout);
                if !captured.is_empty() {
                    result.push_str("\n--- partial output before timeout ---\n");
                    result.push_str(&captured);
                }
                if stdout_truncated || stderr_truncated {
                    result.push_str("\n--- output truncated (exceeded capture cap) ---\n");
                }
                Ok(ToolResult::new("bash", result))
            }
            WaitOutcome::Aborted => {
                let stdout = String::from_utf8_lossy(&captured_stdout);
                let stderr = String::from_utf8_lossy(&captured_stderr);
                let captured = format_captured(&stdout, &stderr);
                let mut result =
                    String::from("Command aborted because the surrounding run was canceled.");
                if !captured.is_empty() {
                    result.push_str("\n--- partial output before cancellation ---\n");
                    result.push_str(&captured);
                }
                Ok(ToolResult::new("bash", result))
            }
        }
    }
}

/// How the child wait ended: naturally, on timeout, or on cancellation of
/// the surrounding run.
enum WaitOutcome {
    Done(std::io::Result<std::process::ExitStatus>),
    TimedOut,
    Aborted,
}

/// Resolves once the surrounding turn is canceled. Polling keeps the bash
/// tool from waiting on a child that would otherwise run to completion
/// long after the user pressed Ctrl-C.
async fn wait_for_abort(ctx: &ToolContext) {
    loop {
        if ctx.is_aborted() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use tempfile::tempdir;
    use tokio::fs;

    #[test]
    fn safe_mode_characterization() {
        for command in [
            "ls", "cat", "grep", "rg", "find", "pwd", "echo", "wc", "head", "tail", "whoami",
            "date", "git", "printf", "sort", "uniq", "diff",
        ] {
            assert_eq!(safe_mode_error(command), None);
        }
        for command in [
            "\n echo hello\r\n",
            "git status",
            "echo '-exec'",
            "find . -execx",
            "echo $HOME",
            "echo ",
        ] {
            assert_eq!(safe_mode_error(command), None, "{command:?}");
        }
        let allowed = "[\"ls\", \"cat\", \"grep\", \"rg\", \"find\", \"pwd\", \"echo\", \"wc\", \"head\", \"tail\", \"whoami\", \"date\", \"git\", \"printf\", \"sort\", \"uniq\", \"diff\"]";
        for (command, first) in [
            ("", ""),
            (" \n ", ""),
            ("rm x; echo hi", "rm"),
            ("/bin/ls", "/bin/ls"),
            ("LS", "LS"),
        ] {
            assert_eq!(
                safe_mode_error(command),
                Some(format!(
                    "Command '{}' is not allowed in safe mode. Allowed commands: {}",
                    first, allowed
                ))
            );
        }
        for command in [
            "echo a\nb",
            "echo a\rb",
            "echo a;b",
            "echo a&&b",
            "echo a||b",
            "echo a|b",
            "echo a&b",
            "echo '&'",
            "echo &>x",
        ] {
            assert_eq!(
                safe_mode_error(command).as_deref(),
                Some(
                    "Complex command chaining (;, &&, ||, |, &, newlines) is not allowed in safe mode."
                )
            );
        }
        for command in [
            "echo a>x",
            "ls 2>x",
            "cat <x",
            "cat <<<x",
            "echo '>'",
            "echo >$(pwd)",
        ] {
            assert_eq!(
                safe_mode_error(command).as_deref(),
                Some("Output redirection (>, >>, <<) is not allowed in safe mode.")
            );
        }
        for command in [
            "echo $(pwd)",
            "echo `pwd`",
            "echo '$(pwd)'",
            "find $(pwd) -exec",
        ] {
            assert_eq!(
                safe_mode_error(command).as_deref(),
                Some("Command substitution ($(...), backticks) is not allowed in safe mode.")
            );
        }
        for command in [
            "find . -exec",
            "find . -execdir",
            "find . -ok",
            "find . -okdir",
            "echo -exec",
        ] {
            assert_eq!(
                safe_mode_error(command).as_deref(),
                Some("Command execution via find (-exec/-ok) is not allowed in safe mode.")
            );
        }
    }

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
        // Pipes are drained only after the child is killed; reading first
        // would block until the 2 s command exited on its own.
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
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), tool.execute(args, &ctx))
                .await?
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("Command timed out"));
        assert!(
            result.output.contains("before-sleep"),
            "partial output captured before the timeout should be preserved"
        );
        Ok(())
    }

    #[tokio::test]
    async fn timeout_kills_background_processes_before_draining() -> Result<()> {
        let dir = tempdir()?;
        let tool = BashTool {
            working_directory: dir.path().to_path_buf(),
            safe_mode: false,
        };
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tool.execute(
                serde_json::json!({
                    "command": "echo stdout; echo stderr >&2; sleep 5 & wait",
                    "timeout": 100
                }),
                &ctx,
            ),
        )
        .await?
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("Command timed out"));
        assert!(result.output.contains("stdout"));
        assert!(result.output.contains("stderr"));
        Ok(())
    }

    #[tokio::test]
    async fn exited_shell_does_not_wait_forever_for_inherited_pipes() -> Result<()> {
        let dir = tempdir()?;
        let tool = BashTool {
            working_directory: dir.path().to_path_buf(),
            safe_mode: false,
        };
        let ctx = ToolContext::new("test-session", "test-msg");
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(2500),
            tool.execute(
                serde_json::json!({"command": "sleep 3 &", "timeout": 100}),
                &ctx,
            ),
        )
        .await?
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("output truncated"));
        assert!(!result.output.contains("Command timed out"));
        Ok(())
    }

    #[tokio::test]
    async fn abort_kills_the_command_promptly() -> Result<()> {
        let dir = tempdir()?;
        let tool = BashTool {
            working_directory: dir.path().to_path_buf(),
            safe_mode: false,
        };
        let sentinel = dir.path().join("finished.txt");
        let ctx = ToolContext::new("test-session", "test-msg");

        let canceller = ctx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            canceller.abort();
        });

        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tool.execute(
                serde_json::json!({
                    "command": format!("echo working; sleep 10; touch {}", sentinel.display()),
                    "timeout": 60000
                }),
                &ctx,
            ),
        )
        .await?
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "cancellation should not wait for the command: {:?}",
            started.elapsed()
        );
        assert!(result.output.contains("aborted"));
        assert!(result.output.contains("working"));

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !sentinel.exists(),
            "the command kept running after cancellation"
        );
        Ok(())
    }

    #[tokio::test]
    async fn abort_kills_background_grandchildren() -> Result<()> {
        let dir = tempdir()?;
        let tool = BashTool {
            working_directory: dir.path().to_path_buf(),
            safe_mode: false,
        };
        let sentinel = dir.path().join("grandchild.txt");
        let ctx = ToolContext::new("test-session", "test-msg");

        let canceller = ctx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            canceller.abort();
        });

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tool.execute(
                serde_json::json!({
                    "command": format!("(sleep 2; touch {}) & wait", sentinel.display()),
                    "timeout": 60000
                }),
                &ctx,
            ),
        )
        .await?
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        assert!(result.output.contains("aborted"));
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        assert!(
            !sentinel.exists(),
            "a background grandchild survived cancellation"
        );
        Ok(())
    }

    #[tokio::test]
    async fn already_canceled_run_does_not_start_the_command() -> Result<()> {
        let dir = tempdir()?;
        let tool = BashTool {
            working_directory: dir.path().to_path_buf(),
            safe_mode: false,
        };
        let sentinel = dir.path().join("started.txt");
        let ctx = ToolContext::new("test-session", "test-msg");
        ctx.abort();
        let result = tool
            .execute(
                serde_json::json!({ "command": format!("touch {}", sentinel.display()) }),
                &ctx,
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("aborted"));
        assert!(!sentinel.exists());
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
