//! Bash command execution tool.
//!
//! This module provides the [`BashTool`] which allows the agent to execute
//! bash commands. It supports both full shell access and a restricted
//! "safe mode" that only allows predefined read-only commands.
//!
//! # Safety Features
//!
//! - **Safe mode**: Optional whitelist of allowed commands (ls, cat, grep, etc.)
//! - **Command chaining prevention**: Blocks `;`, `&&`, `||` operators in safe mode
//! - **Working directory restriction**: Commands run in a specified directory
//! - **Timeout**: Configurable timeout prevents hanging commands
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
use gewu_core::permission::PermissionAction;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::path::PathBuf;

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
                        "Command '{first_word}' is not allowed in safe mode. Allowed commands: {allowed:?}",
                    ),
                ));
            }

            if trimmed.contains(';') || trimmed.contains("&&") || trimmed.contains("||") {
                return Ok(ToolResult::new(
                    "bash",
                    "Complex command chaining (;, &&, ||) is not allowed in safe mode.".to_string(),
                ));
            }

            if trimmed.contains(" >") || trimmed.contains(">>") || trimmed.contains(" <<") {
                return Ok(ToolResult::new(
                    "bash",
                    "Output redirection (>, >>, <<) is not allowed in safe mode.".to_string(),
                ));
            }

            if trimmed.contains("$(") || trimmed.contains("`") {
                return Ok(ToolResult::new(
                    "bash",
                    "Command substitution ($(...), backticks) is not allowed in safe mode."
                        .to_string(),
                ));
            }
        }

        let work_dir = if let Some(wd) = &input.workdir {
            match crate::fs_utils::classify_path_permission(
                PermissionAction::Read,
                &path_from_tool_input(&self.working_directory, wd),
                &crate::fs_utils::configured_read_roots(&self.working_directory),
                &ctx.permission_grants(),
                "bash",
                &ctx.session_id,
                false,
            )
            .map_err(|e| anyhow::anyhow!(e))?
            {
                crate::fs_utils::PathPermissionDecision::Allowed { canonical_path } => canonical_path,
                crate::fs_utils::PathPermissionDecision::Requires(request) => {
                    return Ok(ToolResult::new(
                        "bash",
                        crate::fs_utils::permission_required_result(request).to_string(),
                    ));
                }
                crate::fs_utils::PathPermissionDecision::Denied { reason } => {
                    return Err(anyhow::anyhow!(reason).into());
                }
            }
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

        let child_process = child
            .spawn()
            .context("Failed to spawn bash command")
            .map_err(|e| anyhow::anyhow!(e))?;

        let output_result = tokio::time::timeout(
            std::time::Duration::from_millis(input.timeout),
            child_process.wait_with_output(),
        )
        .await;

        match output_result {
            Ok(io_result) => {
                let output = io_result
                    .context("Failed to wait for child process")
                    .map_err(|e| anyhow::anyhow!(e))?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                if !output.status.success() {
                    let mut result = String::new();
                    if !stdout.is_empty() {
                        result.push_str(&stdout);
                    }
                    if !stderr.is_empty() {
                        if !result.is_empty() {
                            result.push_str("\n--- stderr ---\n");
                        }
                        result.push_str(&stderr);
                    }
                    result.push_str(&format!("\n--- exit status: {} ---\n", output.status));
                    return Err(anyhow::anyhow!("Command failed: {result}").into());
                }

                let mut result = String::new();
                if !stdout.is_empty() {
                    result.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !result.is_empty() {
                        result.push_str("\n--- stderr ---\n");
                    }
                    result.push_str(&stderr);
                }

                Ok(ToolResult::new("bash", result))
            }
            Err(_) => Ok(ToolResult::new(
                "bash",
                format!("Command timed out after {} ms.", input.timeout),
            )),
        }
    }
}

fn path_from_tool_input(working_directory: &std::path::Path, input_path: &str) -> PathBuf {
    let path = std::path::Path::new(input_path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_directory.join(path)
    }
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
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("Command timed out"));
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
        assert!(result.output.contains("Output redirection"));

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

        Ok(())
    }
}
