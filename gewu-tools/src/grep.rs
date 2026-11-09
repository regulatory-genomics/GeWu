//! Grep Tool
//!
//! This tool performs fast content searches across the codebase using regular
//! expressions. It uses the `grep` crate (the library behind ripgrep) for
//! efficient searching and the `ignore` crate for parallel directory walking
//! that respects `.gitignore` files.
//!
//! Output follows opencode v2's ripgrep-backed GrepTool behavior:
//! - a `limit` parameter bounding the number of matches (default 100),
//! - a `limit + 1` collection cap so truncation is detectable,
//! - matched lines truncated to 2000 characters,
//! - results grouped by file,
//! - hidden files searched while `.git` directories are excluded,
//! - a file `path` scopes the search to that single file.

use crate::fs_utils::{into_tool_error, resolve_read_path};
use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use grep::regex::RegexMatcher;
use grep::searcher::{BinaryDetection, SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, WalkState};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Default maximum number of matches shown, mirroring opencode's GrepTool.
const DEFAULT_GREP_LIMIT: usize = 100;
/// Truncate each matched line to this many characters (opencode parity).
const MAX_LINE_CHARS: usize = 2_000;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GrepArgs {
    /// Regex pattern to search for in file contents
    pub pattern: String,
    /// The directory to search in. Defaults to the current working directory.
    pub path: Option<String>,
    /// File pattern to include in the search (e.g. "*.js", "*.{ts,tsx}")
    pub include: Option<String>,
    /// Maximum matches to return
    #[serde(default = "default_grep_limit")]
    pub limit: usize,
}

fn default_grep_limit() -> usize {
    DEFAULT_GREP_LIMIT
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

/// Collects matches from a single file, stopping once the cap is reached.
struct MatchSink {
    matches: Arc<Mutex<Vec<Match>>>,
    path: PathBuf,
    limit: usize,
}

impl Sink for MatchSink {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep::searcher::Searcher,
        mat: &SinkMatch,
    ) -> Result<bool, Self::Error> {
        let mut matches = self
            .matches
            .lock()
            .map_err(|_| std::io::Error::other("grep match lock poisoned"))?;
        if matches.len() > self.limit {
            return Ok(false);
        }

        matches.push(Match {
            path: self.path.to_string_lossy().to_string(),
            line_number: mat.line_number().unwrap_or(0),
            line_text: truncate_line(&String::from_utf8_lossy(mat.bytes())),
        });
        Ok(true)
    }
}

/// Truncates a matched line to [`MAX_LINE_CHARS`] characters with a `...` suffix.
fn truncate_line(text: &str) -> String {
    let trimmed = text.trim_end();
    match trimmed.char_indices().nth(MAX_LINE_CHARS) {
        Some((idx, _)) => format!("{}...", &trimmed[..idx]),
        None => trimmed.to_string(),
    }
}

/// Whether the collection cap is reached, signaling the walker to quit.
fn cap_reached(matches: &Arc<Mutex<Vec<Match>>>, limit: usize) -> bool {
    matches
        .lock()
        .map(|m| m.len() > limit)
        .unwrap_or(false)
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
        if input.pattern.is_empty() {
            return Err(into_tool_error(anyhow::anyhow!("pattern is required")));
        }

        let search_path_str = input.path.as_deref().unwrap_or(".");
        let search_path =
            resolve_read_path(&self.working_directory, search_path_str, ctx.enforcement)
                .await
                .map_err(into_tool_error)?;

        if input.limit == 0 {
            return Err(into_tool_error(anyhow::anyhow!(
                "limit must be greater than 0"
            )));
        }

        let pattern = input.pattern;
        let pattern_for_task = pattern.clone();
        let limit = input.limit;
        let (output, total, truncated) = tokio::task::spawn_blocking(move || {
            run_search(
                &pattern_for_task,
                input.include.as_deref(),
                &search_path,
                limit,
            )
        })
        .await
        .map_err(into_tool_error)??;

        let mut metadata = std::collections::HashMap::new();
        metadata.insert("matches".to_string(), serde_json::json!(total));
        metadata.insert("truncated".to_string(), serde_json::json!(truncated));

        Ok(ToolResult {
            title: pattern,
            output,
            metadata,
            attachments: vec![],
            truncated,
            output_path: None,
        })
    }
}

/// Builds the include-glob override used to filter searched files, matching
/// ripgrep's `--glob=<include>` semantics (gitignore syntax, brace expansion).
fn build_include_override(
    base: &std::path::Path,
    include: &str,
) -> Result<ignore::overrides::Override, Box<dyn Error + Send + Sync>> {
    let mut glob_builder = ignore::overrides::OverrideBuilder::new(base);
    glob_builder.add(include).map_err(into_tool_error)?;
    glob_builder.build().map_err(into_tool_error)
}

/// Searches one file and appends matches. Used when `path` points at a file
/// rather than a directory (opencode v2 scopes the search to that file).
fn search_single_file(
    matcher: &grep::regex::RegexMatcher,
    file: &std::path::Path,
    matches: &Arc<Mutex<Vec<Match>>>,
    limit: usize,
) {
    let sink = MatchSink {
        matches: matches.clone(),
        path: file.to_path_buf(),
        limit,
    };
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();
    if let Err(err) = searcher.search_path(matcher, file, sink) {
        tracing::debug!("grep: search error on {}: {}", file.display(), err);
    }
}

/// Formats collected matches, grouped by file, as opencode's GrepTool does.
fn format_matches(
    matches_guard: &[Match],
    total: usize,
    truncated: bool,
    limit: usize,
) -> String {
    let mut output = format!(
        "Found {} matches{}",
        total,
        if truncated {
            " (more matches available)"
        } else {
            ""
        }
    );
    let mut current_file = "";
    for m in &matches_guard[..limit.min(matches_guard.len())] {
        if current_file != m.path {
            output.push_str("\n\n");
            output.push_str(&m.path);
            output.push(':');
            current_file = &m.path;
        }
        output.push_str(&format!("\n  Line {}: {}", m.line_number, m.line_text));
    }

    if truncated {
        output.push_str("\n\n(Results truncated. Consider using a more specific path or pattern.)");
    }
    output
}

/// Runs the parallel walk + search and formats results grouped by file.
///
/// Mirrors opencode v2's ripgrep invocation: hidden files are searched
/// (`--hidden`), `.git` directories are always excluded
/// (`--glob=!**/.git/**`), and `limit + 1` matches are collected so
/// truncation is detectable.
fn run_search(
    pattern: &str,
    include: Option<&str>,
    search_path: &std::path::Path,
    limit: usize,
) -> Result<(String, usize, bool), Box<dyn Error + Send + Sync>> {
    let matcher = RegexMatcher::new(pattern).map_err(into_tool_error)?;
    let matches = Arc::new(Mutex::new(Vec::new()));

    // opencode v2 scopes the search to a single file when `path` is a file.
    if search_path.is_file() {
        if let Some(inc) = include {
            let base = search_path.parent().unwrap_or(std::path::Path::new("."));
            let override_matcher = build_include_override(base, inc)?;
            if override_matcher.matched(search_path, false).is_ignore() {
                return Ok(("No files found".to_string(), 0, false));
            }
        }
        search_single_file(&matcher, search_path, &matches, limit);
        let guard = matches
            .lock()
            .map_err(|_| into_tool_error(anyhow::anyhow!("grep match lock poisoned")))?;
        let mut sorted = guard.clone();
        sorted.sort_by_key(|m| m.line_number);
        return if sorted.is_empty() {
            Ok(("No files found".to_string(), 0, false))
        } else {
            let total = sorted.len();
            let truncated = total > limit;
            Ok((format_matches(&sorted, total, truncated, limit), total, truncated))
        };
    }

    let mut builder = WalkBuilder::new(search_path);
    // Search hidden files (ripgrep `--hidden`) but never descend into `.git`
    // (ripgrep `--glob=!**/.git/**`).
    builder.hidden(false);
    builder.filter_entry(|entry| entry.file_name() != std::ffi::OsStr::new(".git"));
    if let Some(inc) = include {
        builder.overrides(build_include_override(search_path, inc)?);
    }

    let matches_for_visitor = matches.clone();
    let matcher_for_visitor = matcher.clone();

    builder.build_parallel().run(move || {
        let matches = matches_for_visitor.clone();
        let matcher = matcher_for_visitor.clone();
        Box::new(
            move |result: Result<ignore::DirEntry, ignore::Error>| -> WalkState {
                let Ok(entry) = result else {
                    return WalkState::Continue;
                };
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    return WalkState::Continue;
                }
                if cap_reached(&matches, limit) {
                    return WalkState::Quit;
                }

                let path = entry.path().to_path_buf();
                let sink = MatchSink {
                    matches: matches.clone(),
                    path,
                    limit,
                };
                let mut searcher = SearcherBuilder::new()
                    .binary_detection(BinaryDetection::quit(b'\x00'))
                    .build();
                if let Err(err) = searcher.search_path(&matcher, entry.path(), sink) {
                    tracing::debug!("grep: search error on {}: {}", entry.path().display(), err);
                }

                if cap_reached(&matches, limit) {
                    WalkState::Quit
                } else {
                    WalkState::Continue
                }
            },
        )
    });

    let mut matches_guard = matches
        .lock()
        .map_err(|_| into_tool_error(anyhow::anyhow!("grep match lock poisoned")))?;
    matches_guard.sort_by(|a, b| a.path.cmp(&b.path).then(a.line_number.cmp(&b.line_number)));

    if matches_guard.is_empty() {
        return Ok(("No files found".to_string(), 0, false));
    }

    let total = matches_guard.len();
    let truncated = total > limit;
    let output = format_matches(&matches_guard, total, truncated, limit);

    Ok((output, total, truncated))
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

        assert_eq!(result.title, "hello");
        assert!(result.output.contains("Found 2 matches"));
        assert!(result.output.contains("a.txt:"));
        assert!(result.output.contains("b.rs:"));
        assert!(result.output.contains("  Line 1: fn hello_world() {}"));
        assert!(result.output.contains("  Line 1: pub fn hello_rust() {}"));
        assert_eq!(result.metadata.get("matches"), Some(&serde_json::json!(2)));
        assert_eq!(
            result.metadata.get("truncated"),
            Some(&serde_json::json!(false))
        );
        assert!(!result.truncated);
        Ok(())
    }

    #[tokio::test]
    async fn test_grep_tool_truncation() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();

        for name in ["a.rs", "b.rs", "c.rs"] {
            let mut content = String::new();
            for i in 0..40 {
                content.push_str(&format!("fn hello_{name}_{i}() {{}}\n"));
            }
            fs::write(dir.path().join(name), content).await?;
        }

        let tool = GrepTool {
            working_directory: working_dir,
        };
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(
                serde_json::json!({ "pattern": "hello", "path": dir.path().to_str().unwrap() }),
                &ctx,
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        assert!(result.truncated);
        assert_eq!(
            result.metadata.get("matches"),
            Some(&serde_json::json!(DEFAULT_GREP_LIMIT + 1))
        );
        assert!(
            result
                .output
                .contains("Found 101 matches (more matches available)")
        );
        assert!(
            result
                .output
                .contains("(Results truncated. Consider using a more specific path or pattern.)")
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_grep_tool_limit_param() -> Result<()> {
        let dir = tempdir()?;
        fs::write(dir.path().join("a.txt"), "hello\nhello\nhello\n").await?;

        let tool = GrepTool {
            working_directory: dir.path().to_path_buf(),
        };
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(
                serde_json::json!({ "pattern": "hello", "path": dir.path().to_str().unwrap(), "limit": 2 }),
                &ctx,
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        assert!(result.truncated);
        assert_eq!(result.metadata.get("matches"), Some(&serde_json::json!(3)));
        assert!(result.output.contains("Found 3 matches (more matches available)"));
        assert_eq!(result.output.matches("  Line ").count(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_grep_tool_single_file_path() -> Result<()> {
        let dir = tempdir()?;
        fs::write(dir.path().join("a.txt"), "hello here\n").await?;
        fs::write(dir.path().join("b.txt"), "hello there\n").await?;

        let tool = GrepTool {
            working_directory: dir.path().to_path_buf(),
        };
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(
                serde_json::json!({
                    "pattern": "hello",
                    "path": dir.path().join("a.txt").to_str().unwrap()
                }),
                &ctx,
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        assert!(result.output.contains("a.txt:"));
        assert!(!result.output.contains("b.txt"), "{}", result.output);
        assert_eq!(result.metadata.get("matches"), Some(&serde_json::json!(1)));
        Ok(())
    }

    #[tokio::test]
    async fn test_grep_tool_hidden_files_searched_git_excluded() -> Result<()> {
        let dir = tempdir()?;
        fs::create_dir(dir.path().join(".git")).await?;
        fs::write(dir.path().join(".git").join("config"), "secret in git\n").await?;
        fs::write(dir.path().join(".hidden"), "secret in hidden\n").await?;
        fs::write(dir.path().join("visible.txt"), "secret in visible\n").await?;

        let tool = GrepTool {
            working_directory: dir.path().to_path_buf(),
        };
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(
                serde_json::json!({ "pattern": "secret", "path": dir.path().to_str().unwrap() }),
                &ctx,
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        assert!(result.output.contains("visible.txt"), "{}", result.output);
        assert!(result.output.contains(".hidden"), "{}", result.output);
        assert!(
            !result.output.contains("config"),
            ".git contents must be excluded: {}",
            result.output
        );
        assert_eq!(result.metadata.get("matches"), Some(&serde_json::json!(2)));
        Ok(())
    }

    #[tokio::test]
    async fn test_grep_tool_no_matches() -> Result<()> {
        let dir = tempdir()?;
        fs::write(dir.path().join("a.txt"), "nothing relevant\n").await?;

        let tool = GrepTool {
            working_directory: dir.path().to_path_buf(),
        };
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(
                serde_json::json!({ "pattern": "zzz-not-there", "path": dir.path().to_str().unwrap() }),
                &ctx,
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        assert_eq!(result.output, "No files found");
        assert_eq!(result.metadata.get("matches"), Some(&serde_json::json!(0)));
        assert!(!result.truncated);
        Ok(())
    }

    #[tokio::test]
    async fn test_grep_tool_include_filter() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();

        fs::write(dir.path().join("a.txt"), "hello world\n").await?;
        fs::write(dir.path().join("b.rs"), "hello rust\n").await?;
        fs::write(dir.path().join("c.py"), "hello python\n").await?;

        let tool = GrepTool {
            working_directory: working_dir,
        };
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(
                serde_json::json!({
                    "pattern": "hello",
                    "path": dir.path().to_str().unwrap(),
                    "include": "*.rs"
                }),
                &ctx,
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        assert!(result.output.contains("b.rs:"));
        assert!(!result.output.contains("a.txt"));
        assert!(!result.output.contains("c.py"));
        Ok(())
    }

    #[tokio::test]
    async fn test_grep_tool_invalid_glob() -> Result<()> {
        let dir = tempdir()?;
        let tool = GrepTool {
            working_directory: dir.path().to_path_buf(),
        };
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(
                serde_json::json!({
                    "pattern": "hello",
                    "path": dir.path().to_str().unwrap(),
                    "include": "["
                }),
                &ctx,
            )
            .await;

        assert!(result.is_err(), "invalid include glob should be an error");
        Ok(())
    }

    #[tokio::test]
    async fn test_grep_tool_long_line_truncation() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();
        fs::write(
            dir.path().join("long.txt"),
            format!("{}\n", "a".repeat(2500)),
        )
        .await?;

        let tool = GrepTool {
            working_directory: working_dir,
        };
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(
                serde_json::json!({ "pattern": "a", "path": dir.path().to_str().unwrap() }),
                &ctx,
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        assert!(result.output.contains("..."));
        assert!(
            !result.output.contains(&"a".repeat(2001)),
            "matched line text must be truncated"
        );
        Ok(())
    }
}
