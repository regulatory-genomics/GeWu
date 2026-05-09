//! Grep Tool
//!
//! This tool performs fast content searches across the codebase using regular expressions.
//! It uses the `ignore` crate to respect `.gitignore` files and the `grep` crate for
//! efficient searching.

use crate::fs_utils::{
    PathPermissionDecision, classify_path_permission, configured_read_roots, into_tool_error,
    permission_required_result,
};
use anyhow::Result;
use async_trait::async_trait;
use gewu_core::permission::PermissionAction;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use grep::regex::RegexMatcher;
use grep::searcher::{BinaryDetection, SearcherBuilder, Sink, SinkMatch};
use ignore::WalkBuilder;
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GrepArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub include: Option<String>,
}

pub struct GrepTool {
    pub working_directory: PathBuf,
}

#[derive(Clone)]
struct Match {
    path: String,
    line_number: u64,
    line_text: String,
}

struct MatchCollector {
    matches: Arc<Mutex<Vec<Match>>>,
    path: PathBuf,
}

impl Sink for MatchCollector {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep::searcher::Searcher,
        mat: &SinkMatch,
    ) -> Result<bool, Self::Error> {
        let line_number = mat.line_number().unwrap_or(0);
        let line_text = String::from_utf8_lossy(mat.bytes()).to_string();

        let m = Match {
            path: self.path.to_string_lossy().to_string(),
            line_number,
            line_text: line_text.trim_end().to_string(),
        };

        if let Ok(mut matches) = self.matches.lock() {
            if matches.len() < 1000 {
                matches.push(m);
            } else {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        include_str!("prompts/grep.txt")
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(GrepArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: GrepArgs = serde_json::from_value(args).map_err(into_tool_error)?;
        let search_path_str = input.path.as_deref().unwrap_or(".");

        let search_path = match classify_path_permission(
            PermissionAction::Read,
            &path_from_tool_input(&self.working_directory, search_path_str),
            &configured_read_roots(&self.working_directory),
            &ctx.permission_grants(),
            "grep",
            &ctx.session_id,
            false,
        )
        .map_err(into_tool_error)?
        {
            PathPermissionDecision::Allowed { canonical_path } => canonical_path,
            PathPermissionDecision::Requires(request) => {
                return Ok(ToolResult::new(
                    "grep",
                    permission_required_result(request).to_string(),
                ));
            }
            PathPermissionDecision::Denied { reason } => return Err(into_tool_error(anyhow::anyhow!(reason))),
        };

        let result =
            tokio::task::spawn_blocking(move || -> Result<String, Box<dyn Error + Send + Sync>> {
                let matcher = RegexMatcher::new(&input.pattern).map_err(into_tool_error)?;
                let matches = Arc::new(Mutex::new(Vec::new()));

                let mut builder = WalkBuilder::new(&search_path);
                if let Some(inc) = &input.include {
                    let mut glob_builder = ignore::overrides::OverrideBuilder::new(&search_path);
                    if glob_builder.add(inc).is_ok()
                        && let Ok(overrides) = glob_builder.build()
                    {
                        builder.overrides(overrides);
                    }
                }

                let walker = builder.build();

                let matches_clone_outer = matches.clone();
                let matcher_clone_outer = matcher.clone();

                for entry in walker
                    .filter_map(|result| result.ok())
                    .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_file()))
                {
                    let path = entry.path().to_path_buf();
                    let matches_clone = matches_clone_outer.clone();
                    let matcher_clone = matcher_clone_outer.clone();

                    let collector = MatchCollector {
                        matches: matches_clone,
                        path: path.clone(),
                    };

                    let mut searcher = SearcherBuilder::new()
                        .binary_detection(BinaryDetection::quit(b'\x00'))
                        .build();

                    let _ = searcher.search_path(&matcher_clone, &path, collector);
                }

                let matches_guard = matches.lock().unwrap();
                if matches_guard.is_empty() {
                    return Ok("No matches found.".to_string());
                }

                let limit = 100;
                let truncated = matches_guard.len() > limit;
                let display_matches = if truncated {
                    &matches_guard[..limit]
                } else {
                    &matches_guard[..]
                };

                let mut output = display_matches
                    .iter()
                    .map(|m| format!("{}:{}: {}", m.path, m.line_number, m.line_text))
                    .collect::<Vec<String>>()
                    .join("\n");

                if !output.is_empty() {
                    output.push('\n');
                }

                if truncated {
                    output.push_str(&format!(
                        "\n... and {} more matches not shown.",
                        matches_guard.len() - limit
                    ));
                }

                Ok(output)
            })
            .await
            .map_err(into_tool_error)??;

        Ok(ToolResult::new("grep", result))
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
    async fn test_grep_tool() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();
        let file1 = dir.path().join("a.txt");
        let file2 = dir.path().join("b.rs");

        fs::write(&file1, "fn hello_world() {}").await?;
        fs::write(&file2, "pub fn hello_rust() {}").await?;

        let tool = GrepTool {
            working_directory: working_dir,
        };

        let args = serde_json::json!({
            "pattern": "hello",
            "path": dir.path().to_str().unwrap()
        });
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        assert!(result.output.contains("a.txt"));
        assert!(result.output.contains("b.rs"));
        Ok(())
    }
}
