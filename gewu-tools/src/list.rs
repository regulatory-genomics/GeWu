//! File Listing Tool
//!
//! This tool lists files and directories in a specified path, similar to the `ls` command
//! but with recursive capabilities.

use crate::fs_utils::{into_tool_error, resolve_safe_path};
use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use ignore::WalkBuilder;
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListArgs {
    pub path: Option<String>,
    pub ignore: Option<Vec<String>>,
}

pub struct ListTool {
    pub working_directory: PathBuf,
}

#[async_trait]
impl Tool for ListTool {
    fn name(&self) -> &str {
        "list"
    }

    fn description(&self) -> &str {
        include_str!("prompts/ls.txt")
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(ListArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: ListArgs = serde_json::from_value(args).map_err(into_tool_error)?;

        let path_str = input.path.as_deref().unwrap_or(".");

        let root_path =
            resolve_safe_path(&self.working_directory, path_str).map_err(into_tool_error)?;

        let mut builder = WalkBuilder::new(&root_path);
        builder.hidden(false);
        builder.git_ignore(true);

        if let Some(ignores) = &input.ignore {
            let mut overrides = ignore::overrides::OverrideBuilder::new(&root_path);
            for ignore in ignores {
                let _ = overrides.add(&format!("!{}", ignore));
            }
            if let Ok(ov) = overrides.build() {
                builder.overrides(ov);
            }
        }

        let limit = 500;
        let mut files: Vec<String> = builder
            .build()
            .filter_map(|result| result.ok())
            .filter_map(|entry| {
                let path = entry.path();
                if path == root_path {
                    return None;
                }

                let relative = path.strip_prefix(&root_path).unwrap_or(path);
                let display = relative.to_string_lossy().to_string();
                let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
                Some(if is_dir {
                    format!("{}/", display)
                } else {
                    display
                })
            })
            .take(limit + 1)
            .collect();

        let truncated = files.len() > limit;
        if truncated {
            files.truncate(limit);
            files.push("... (truncated)".to_string());
        }

        files.sort();
        Ok(ToolResult::new("list", files.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use tempfile::tempdir;
    use tokio::fs;

    #[tokio::test]
    async fn test_list_tool() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();

        fs::write(dir.path().join("a.txt"), "").await?;
        let sub_dir = dir.path().join("sub");
        fs::create_dir(&sub_dir).await?;
        fs::write(sub_dir.join("b.rs"), "").await?;

        let tool = ListTool {
            working_directory: working_dir,
        };

        let args = serde_json::json!({});
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        assert!(result.output.contains("a.txt"));
        assert!(result.output.contains("sub/"));
        Ok(())
    }
}
