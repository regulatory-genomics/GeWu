//! Command-backed tool: spawns an external process, passes JSON via stdin, reads JSON from stdout.

use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// A tool backed by an external command.
///
/// On execute: serializes input to JSON, writes to process stdin,
/// reads stdout as the result. Non-zero exit → error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandToolDef {
    pub name: String,
    pub description: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
}

fn default_timeout() -> u64 {
    30_000
}

pub struct CommandTool {
    pub def: CommandToolDef,
    schema: RootSchema,
}

impl CommandTool {
    pub fn new(def: CommandToolDef) -> Self {
        let schema = if let Some(ref s) = def.input_schema {
            serde_json::from_value(s.clone()).unwrap_or_default()
        } else {
            RootSchema::default()
        };
        Self { def, schema }
    }
}

#[async_trait]
impl Tool for CommandTool {
    fn name(&self) -> &str {
        &self.def.name
    }

    fn description(&self) -> &str {
        &self.def.description
    }

    fn schema(&self) -> RootSchema {
        self.schema.clone()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let input_json = serde_json::to_string(&args)?;

        let mut cmd = Command::new(&self.def.command);
        cmd.args(&self.def.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(ref wd) = self.def.working_dir {
            cmd.current_dir(wd);
        }
        for (k, v) in &self.def.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "Failed to spawn command '{}': {}",
                self.def.command, e
            )
        })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input_json.as_bytes()).await?;
            drop(stdin);
        }

        let output = tokio::time::timeout(
            std::time::Duration::from_millis(self.def.timeout_ms),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| {
            format!(
                "Command '{}' timed out after {}ms",
                self.def.name, self.def.timeout_ms
            )
        })??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "Command '{}' exited with {}: {}",
                self.def.name,
                output.status,
                stderr.trim()
            )
            .into());
        }

        let stdout = String::from_utf8(output.stdout)?;
        Ok(ToolResult::new(self.def.name.clone(), stdout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_core::types::ToolContext;
    use serde_json::json;

    fn test_ctx() -> ToolContext {
        ToolContext::new("test", "test")
    }

    #[tokio::test]
    async fn command_tool_echo() {
        let def = CommandToolDef {
            name: "echo_test".into(),
            description: "echoes input".into(),
            command: "cat".into(),
            args: vec![],
            working_dir: None,
            env: HashMap::new(),
            timeout_ms: 5000,
            input_schema: None,
        };
        let tool = CommandTool::new(def);
        let result = tool
            .execute(json!({"hello": "world"}), &test_ctx())
            .await
            .unwrap();
        let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output, json!({"hello": "world"}));
    }

    #[tokio::test]
    async fn command_tool_with_args() {
        let def = CommandToolDef {
            name: "bash_test".into(),
            description: "runs bash".into(),
            command: "bash".into(),
            args: vec!["-c".into(), "cat | jq '.x'".into()],
            working_dir: None,
            env: HashMap::new(),
            timeout_ms: 5000,
            input_schema: None,
        };
        let tool = CommandTool::new(def);
        let result = tool.execute(json!({"x": 42}), &test_ctx()).await.unwrap();
        assert_eq!(result.output.trim(), "42");
    }

    #[tokio::test]
    async fn command_tool_nonzero_exit() {
        let def = CommandToolDef {
            name: "fail_test".into(),
            description: "fails".into(),
            command: "bash".into(),
            args: vec!["-c".into(), "echo 'oops' >&2; exit 1".into()],
            working_dir: None,
            env: HashMap::new(),
            timeout_ms: 5000,
            input_schema: None,
        };
        let tool = CommandTool::new(def);
        let err = tool
            .execute(json!({}), &test_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("oops"));
    }

    #[tokio::test]
    async fn command_tool_timeout() {
        let def = CommandToolDef {
            name: "slow_test".into(),
            description: "sleeps".into(),
            command: "sleep".into(),
            args: vec!["10".into()],
            working_dir: None,
            env: HashMap::new(),
            timeout_ms: 100,
            input_schema: None,
        };
        let tool = CommandTool::new(def);
        let err = tool
            .execute(json!({}), &test_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }
}
