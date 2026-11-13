//! Filesystem Tools
//!
//! This module implements tools for interacting with the local filesystem.
//! All file operations are restricted to a configured working directory
//! for security.
//!
//! # Security
//!
//! File paths are resolved according to the [`PathEnforcement`] level carried
//! on the [`ToolContext`]. The level is server configuration (config key
//! `sandbox`) and defaults to `None`: paths are used as provided with no
//! containment checks. `Contain` rejects paths escaping the working directory
//! via [`resolve_safe_path`](crate::fs_utils::resolve_safe_path), and
//! `Strict` additionally canonicalizes and rejects symlinks.
//!
//! # Available Tools
//!
//! - [`ReadFileTool`] - Reads content from files or lists directories
//! - [`WriteFileTool`] - Writes content to files (overwrites existing)
//! - [`EditFileTool`] - Modifies existing files by replacing text
//! - [`GlobTool`] - Finds files using glob patterns, respects .gitignore

use crate::fs_utils::{
    glob_with_gitignore, into_tool_error, is_binary_file, mime_type, read_bytes, read_text,
    resolve_read_path, resolve_write_path, write,
};
use crate::replacers::smart_replace;
use anyhow::Result;
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use gewu_core::traits::Tool;
use gewu_core::types::{FileAttachment, ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::path::PathBuf;
use tokio::io::BufReader;

const DEFAULT_READ_LIMIT: usize = 2000;
const MAX_LINE_LENGTH: usize = 2000;
const MAX_LINE_SUFFIX: &str = "... (line truncated to 2000 chars)";
const MAX_BYTES: usize = 50 * 1024;
const MAX_BYTES_LABEL: &str = "50 KB";
/// Cap for image/PDF reads (raw bytes, before base64 inflation).
const MAX_MEDIA_BYTES: usize = 20 * 1024 * 1024;
/// Default cap on the number of files returned by [`GlobTool`].
const DEFAULT_GLOB_LIMIT: usize = 100;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ReadArgs {
    pub file_path: String,
    #[serde(default = "default_read_limit")]
    pub limit: usize,
    #[serde(default = "default_offset")]
    pub offset: usize,
}

fn default_read_limit() -> usize {
    DEFAULT_READ_LIMIT
}

fn default_offset() -> usize {
    1
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WriteArgs {
    pub file_path: String,
    pub content: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct EditArgs {
    pub file_path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GlobArgs {
    /// The glob pattern to match files against
    pub pattern: String,
    /// The directory to search in. If not specified, the current working
    /// directory will be used. IMPORTANT: Omit this field to use the default
    /// directory. DO NOT enter "undefined" or "null" - simply omit it for the
    /// default behavior. Must be a valid directory path if provided.
    pub path: Option<String>,
    /// Maximum number of results to return
    #[serde(default = "default_glob_limit")]
    pub limit: usize,
    /// Include hidden files (dot-files) in the results. Hidden files are
    /// excluded by default unless listed in .gitignore rules allow them.
    #[serde(default)]
    pub hidden: bool,
}

fn default_glob_limit() -> usize {
    DEFAULT_GLOB_LIMIT
}

pub struct ReadFileTool {
    pub working_directory: PathBuf,
}

pub struct WriteFileTool {
    pub working_directory: PathBuf,
}

pub struct EditFileTool {
    pub working_directory: PathBuf,
}

pub struct GlobTool {
    pub working_directory: PathBuf,
}

fn truncate_read_line(text: &str) -> String {
    if text.len() > MAX_LINE_LENGTH {
        let mut cut = MAX_LINE_LENGTH;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}{}", &text[..cut], MAX_LINE_SUFFIX)
    } else {
        text.to_string()
    }
}

fn render_read_text(
    input: &ReadArgs,
    lines: &[String],
    total_lines: usize,
    truncated_by_bytes: bool,
    has_more_lines: bool,
) -> String {
    if total_lines < input.offset && !(total_lines == 0 && input.offset == 1) {
        return format!(
            "Offset {} is out of range for this file ({} lines)",
            input.offset, total_lines
        );
    }
    let start = input.offset.saturating_sub(1);
    let content: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{}: {}", start + i + 1, line))
        .collect();
    let last_read_line = start + lines.len().max(1);
    let next_offset = last_read_line + 1;
    let mut output = format!(
        "<path>{}</path>\n<type>file</type>\n<content>\n{}\n</content>",
        input.file_path,
        content.join("\n")
    );
    if truncated_by_bytes {
        output.push_str(&format!(
            "\n\n(Output capped at {}. Showing lines {}-{}. Use offset={} to continue.)",
            MAX_BYTES_LABEL, input.offset, last_read_line, next_offset
        ));
    } else if has_more_lines {
        output.push_str(&format!(
            "\n\n(Showing lines {}-{} of {}. Use offset={} to continue.)",
            input.offset, last_read_line, total_lines, next_offset
        ));
    } else {
        output.push_str(&format!("\n\n(End of file - total {} lines)", total_lines));
    }
    output
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        include_str!("prompts/read.txt")
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(ReadArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: ReadArgs = serde_json::from_value(args).map_err(into_tool_error)?;

        let file_path =
            resolve_read_path(&self.working_directory, &input.file_path, ctx.enforcement)
                .await
                .map_err(into_tool_error)?;

        let metadata = tokio::fs::metadata(&file_path).await;

        if metadata.is_err() {
            let parent = file_path.parent().unwrap_or(file_path.as_path());
            let basename = file_path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.rsplit('.').next())
                .unwrap_or("");

            if tokio::fs::metadata(parent).await.is_ok() {
                let mut suggestions: Vec<String> = Vec::new();
                let mut dir_stream = tokio::fs::read_dir(parent).await.map_err(into_tool_error)?;

                while let Some(entry) = dir_stream.next_entry().await.map_err(into_tool_error)? {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let name_lower = name.to_lowercase();
                    let base_lower = basename.to_lowercase();
                    if name_lower.contains(&base_lower) || base_lower.contains(&name_lower) {
                        suggestions.push(name);
                    }
                    if suggestions.len() >= 3 {
                        break;
                    }
                }

                if !suggestions.is_empty() {
                    return Ok(ToolResult::new(
                        "read_file",
                        format!(
                            "File not found: {}\n\nDid you mean one of these?\n{}",
                            input.file_path,
                            suggestions.join("\n")
                        ),
                    ));
                }
            }

            return Ok(ToolResult::new(
                "read_file",
                format!("File not found: {}", input.file_path),
            ));
        }

        let meta = metadata.unwrap();

        if meta.is_dir() {
            let mut entries: Vec<String> = Vec::new();
            let mut dir_stream = tokio::fs::read_dir(&file_path)
                .await
                .map_err(into_tool_error)?;

            while let Some(entry) = dir_stream.next_entry().await.map_err(into_tool_error)? {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();

                if path.is_symlink() {
                    let sym_meta = tokio::fs::symlink_metadata(&path).await.ok();
                    if sym_meta.as_ref().is_some_and(|m| m.is_dir()) {
                        entries.push(format!("{}/", name));
                        continue;
                    }
                }

                if path.is_dir() {
                    entries.push(format!("{}/", name));
                } else {
                    entries.push(name);
                }
            }

            entries.sort();

            let start = input.offset.saturating_sub(1);
            let sliced: Vec<&str> = entries
                .iter()
                .skip(start)
                .take(input.limit)
                .map(|s| s.as_str())
                .collect();
            let truncated = start + sliced.len() < entries.len();

            let output = if truncated {
                format!(
                    "<path>{}</path>\n<type>directory</type>\n<entries>\n{}\n</entries>\n\n(Showing {} of {} entries. Use 'offset' parameter to read beyond entry {})",
                    input.file_path,
                    sliced.join("\n"),
                    sliced.len(),
                    entries.len(),
                    input.offset + sliced.len()
                )
            } else {
                format!(
                    "<path>{}</path>\n<type>directory</type>\n<entries>\n{}\n</entries>\n\n({} entries)",
                    input.file_path,
                    sliced.join("\n"),
                    entries.len()
                )
            };

            return Ok(ToolResult::new("read_file", output));
        }

        let mime = mime_type(&file_path);
        let is_image = mime.starts_with("image/")
            && mime != "image/svg+xml"
            && mime != "image/vnd.fastbidsheet";
        let is_pdf = mime == "application/pdf";

        if is_image || is_pdf {
            // Cap media reads like the text path: a multi-gigabyte file with
            // an image/PDF extension would otherwise be fully loaded and
            // base64-inflated in memory.
            if meta.len() > MAX_MEDIA_BYTES as u64 {
                return Ok(ToolResult::new(
                    "read_file",
                    format!(
                        "Cannot read file: {} is {} bytes, which exceeds the {} byte media read cap",
                        input.file_path,
                        meta.len(),
                        MAX_MEDIA_BYTES
                    ),
                ));
            }
            let data = read_bytes(&file_path).await.map_err(into_tool_error)?;
            let encoded = BASE64.encode(&data);
            let msg = if is_image { "Image" } else { "PDF" };
            let result = serde_json::json!({
                "type": "media",
                "mime": mime,
                "data": encoded,
                "message": format!("{} read successfully", msg)
            })
            .to_string();
            return Ok(ToolResult {
                title: "read_file".into(),
                output: result,
                metadata: Default::default(),
                attachments: vec![FileAttachment {
                    path: file_path,
                    mime: Some(mime),
                    size: data.len() as u64,
                }],
                truncated: false,
                output_path: None,
            });
        }

        let is_binary = is_binary_file(&file_path, meta.len())
            .await
            .map_err(into_tool_error)?;
        if is_binary {
            return Ok(ToolResult::new(
                "read_file",
                format!("Cannot read binary file: {}", input.file_path),
            ));
        }

        let start = input.offset.saturating_sub(1);

        let file = tokio::fs::File::open(&file_path)
            .await
            .map_err(into_tool_error)?;
        let reader = BufReader::new(file);
        let mut lines: Vec<String> = Vec::new();
        let mut total_lines: usize = 0;
        let mut bytes: usize = 0;
        let mut truncated_by_bytes = false;
        let mut has_more_lines = false;

        let mut reader = reader;
        let mut line_iter = tokio::io::AsyncBufReadExt::lines(&mut reader);

        while let Some(text) = line_iter.next_line().await.map_err(into_tool_error)? {
            total_lines += 1;
            if total_lines <= start {
                continue;
            }

            if lines.len() >= input.limit {
                has_more_lines = true;
                continue;
            }

            let truncated_line = truncate_read_line(&text);

            let line_size = truncated_line.len() + 1;
            if bytes + line_size > MAX_BYTES {
                truncated_by_bytes = true;
                has_more_lines = true;
                break;
            }

            lines.push(truncated_line);
            bytes += line_size;
        }

        Ok(ToolResult::new(
            "read_file",
            render_read_text(
                &input,
                &lines,
                total_lines,
                truncated_by_bytes,
                has_more_lines,
            ),
        ))
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        include_str!("prompts/write.txt")
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(WriteArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: WriteArgs = serde_json::from_value(args).map_err(into_tool_error)?;

        let file_path =
            resolve_write_path(&self.working_directory, &input.file_path, ctx.enforcement)
                .await
                .map_err(into_tool_error)?;

        write(&file_path, input.content.as_bytes())
            .await
            .map_err(into_tool_error)?;
        Ok(ToolResult::new(
            "write_file",
            format!("Successfully wrote to {}", file_path.to_string_lossy()),
        ))
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        include_str!("prompts/edit.txt")
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(EditArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: EditArgs = serde_json::from_value(args).map_err(into_tool_error)?;

        let file_path =
            resolve_write_path(&self.working_directory, &input.file_path, ctx.enforcement)
                .await
                .map_err(into_tool_error)?;

        let content = read_text(&file_path).await.map_err(into_tool_error)?;

        let new_content = smart_replace(
            &content,
            &input.old_string,
            &input.new_string,
            input.replace_all,
        )
        .map_err(|e| anyhow::anyhow!("{}", e))?;

        write(&file_path, new_content.as_bytes())
            .await
            .map_err(into_tool_error)?;
        Ok(ToolResult::new(
            "edit_file",
            format!("Successfully modified {}", file_path.to_string_lossy()),
        ))
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        include_str!("prompts/glob.txt")
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(GlobArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: GlobArgs = serde_json::from_value(args).map_err(into_tool_error)?;
        if input.limit == 0 {
            return Err(into_tool_error(anyhow::anyhow!(
                "limit must be greater than 0"
            )));
        }

        let search_dir = if let Some(p) = &input.path {
            resolve_read_path(&self.working_directory, p, ctx.enforcement)
                .await
                .map_err(into_tool_error)?
        } else {
            self.working_directory.clone()
        };

        if !search_dir.is_dir() {
            return Err(into_tool_error(anyhow::anyhow!(
                "glob path must be a directory: {}",
                search_dir.display()
            )));
        }

        let (search_dir, pattern, hidden, limit) = (
            search_dir.clone(),
            input.pattern.clone(),
            input.hidden,
            input.limit,
        );
        let workdir = self.working_directory.clone();
        let contain = ctx.enforcement != gewu_core::types::PathEnforcement::None;
        let matches = tokio::task::spawn_blocking(move || {
            let mut matches = glob_with_gitignore(&search_dir, &pattern, hidden, limit)?;
            if contain {
                // The walker follows symlinks, so drop entries that
                // resolve (lexically) outside the working directory.
                matches.entries.retain(|(path, _)| {
                    crate::fs_utils::normalize_path(path).starts_with(&workdir)
                });
            }
            Ok::<_, anyhow::Error>(matches)
        })
        .await
        .map_err(into_tool_error)?
        .map_err(into_tool_error)?;

        let truncated = matches.truncated;
        let count = matches.entries.len();

        let mut output_lines: Vec<String> = matches
            .entries
            .iter()
            .map(|(path, _)| path.to_string_lossy().into_owned())
            .collect();
        if output_lines.is_empty() {
            output_lines.push("No files found".to_string());
        } else if truncated {
            output_lines.push(String::new());
            output_lines.push(format!(
                "(Results are truncated: showing first {count} results. Consider using a more specific path or pattern.)"
            ));
        }

        let mut metadata = std::collections::HashMap::new();
        metadata.insert("count".to_string(), serde_json::json!(count));
        metadata.insert("truncated".to_string(), serde_json::json!(truncated));

        Ok(ToolResult {
            title: input.pattern,
            output: output_lines.join("\n"),
            metadata,
            attachments: vec![],
            truncated,
            output_path: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_line_truncation_preserves_byte_limit_and_utf8() {
        assert_eq!(truncate_read_line("你好"), "你好");
        assert_eq!(truncate_read_line(&"a".repeat(2000)), "a".repeat(2000));
        assert_eq!(
            truncate_read_line(&format!("{}界", "a".repeat(1999))),
            format!("{}{}", "a".repeat(1999), MAX_LINE_SUFFIX)
        );
        assert_eq!(
            truncate_read_line(&"界".repeat(667)),
            format!("{}{}", "界".repeat(666), MAX_LINE_SUFFIX)
        );
    }

    #[test]
    fn read_text_rendering_characterization() {
        let mut input = ReadArgs {
            file_path: "file.txt".into(),
            limit: 2,
            offset: 1,
        };
        let header = "<path>file.txt</path>\n<type>file</type>\n<content>\n";
        assert_eq!(
            render_read_text(&input, &[], 0, false, false),
            format!("{header}\n</content>\n\n(End of file - total 0 lines)")
        );
        let lines = vec!["你好".into(), "".into()];
        assert_eq!(
            render_read_text(&input, &lines, 2, false, false),
            format!("{header}1: 你好\n2: \n</content>\n\n(End of file - total 2 lines)")
        );
        input.offset = 3;
        assert_eq!(
            render_read_text(&input, &lines, 8, false, true),
            format!(
                "{header}3: 你好\n4: \n</content>\n\n(Showing lines 3-4 of 8. Use offset=5 to continue.)"
            )
        );
        assert_eq!(
            render_read_text(&input, &lines, 5, true, false),
            format!(
                "{header}3: 你好\n4: \n</content>\n\n(Output capped at 50 KB. Showing lines 3-4. Use offset=5 to continue.)"
            )
        );
        assert_eq!(
            render_read_text(&input, &[], 2, false, false),
            "Offset 3 is out of range for this file (2 lines)"
        );
        input.offset = 0;
        assert_eq!(
            render_read_text(&input, &[], 1, false, true),
            format!("{header}\n</content>\n\n(Showing lines 0-1 of 1. Use offset=2 to continue.)")
        );
        input.offset = 1;
        input.limit = 0;
        assert_eq!(
            render_read_text(&input, &[], 3, false, true),
            format!("{header}\n</content>\n\n(Showing lines 1-1 of 3. Use offset=2 to continue.)")
        );
    }
    use anyhow::Result;
    use gewu_core::types::PathEnforcement;
    use tempfile::tempdir;
    use tokio::fs;

    #[tokio::test]
    async fn test_read_file_tool() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();
        let file_path = dir.path().join("test_read.txt");
        fs::write(&file_path, "Hello from file").await?;

        let tool = ReadFileTool {
            working_directory: working_dir,
        };
        let args = serde_json::json!({
            "file_path": file_path.to_str().unwrap()
        });
        let ctx = ToolContext::new("test", "msg");

        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("<path>"));
        assert!(result.output.contains("<type>file</type>"));
        assert!(result.output.contains("1: Hello from file"));
        Ok(())
    }

    #[tokio::test]
    async fn test_write_file_tool() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();
        let file_path = dir.path().join("test_write.txt");

        let tool = WriteFileTool {
            working_directory: working_dir,
        };
        let args = serde_json::json!({
            "file_path": file_path.to_str().unwrap(),
            "content": "Hello written"
        });
        let ctx = ToolContext::new("test", "msg");

        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains("Successfully wrote"));

        let content = fs::read_to_string(&file_path).await?;
        assert_eq!(content, "Hello written");
        Ok(())
    }

    #[tokio::test]
    async fn test_glob_tool() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();
        let file1 = dir.path().join("a.txt");
        let file2 = dir.path().join("b.txt");
        let file3 = dir.path().join("c.rs");
        fs::write(&file1, "").await?;
        fs::write(&file2, "").await?;
        fs::write(&file3, "").await?;
        // Equal mtimes exercise the alphabetical tie-break independent of filesystem timing.
        for path in [&file1, &file2] {
            std::fs::File::options().write(true).open(path)?.set_times(
                std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH),
            )?;
        }

        let tool = GlobTool {
            working_directory: working_dir,
        };
        let args = serde_json::json!({
            "pattern": "*.txt"
        });
        let ctx = ToolContext::new("test", "msg");

        let result = tool
            .execute(args, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert_eq!(result.metadata.get("count"), Some(&serde_json::json!(2)));
        assert_eq!(
            result.metadata.get("truncated"),
            Some(&serde_json::json!(false))
        );
        assert!(!result.truncated);
        let lines: Vec<&str> = result.output.lines().collect();
        assert_eq!(lines.len(), 2);
        // Walker order is filesystem-dependent; only membership is stable.
        let names: Vec<&str> = lines
            .iter()
            .map(|line| line.rsplit('/').next().expect("non-empty path"))
            .collect();
        assert!(names.contains(&"a.txt"), "{}", result.output);
        assert!(names.contains(&"b.txt"), "{}", result.output);
        Ok(())
    }

    #[tokio::test]
    async fn glob_no_matches_says_no_files_found() -> Result<()> {
        let dir = tempdir()?;
        let tool = GlobTool {
            working_directory: dir.path().to_path_buf(),
        };
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(serde_json::json!({ "pattern": "*.md" }), &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert_eq!(result.output, "No files found");
        assert_eq!(result.metadata.get("count"), Some(&serde_json::json!(0)));
        Ok(())
    }

    #[tokio::test]
    async fn glob_respects_limit_and_reports_truncation() -> Result<()> {
        let dir = tempdir()?;
        for i in 0..5 {
            fs::write(dir.path().join(format!("f{i}.txt")), "").await?;
        }
        let tool = GlobTool {
            working_directory: dir.path().to_path_buf(),
        };
        let ctx = ToolContext::new("test", "msg");

        let result = tool
            .execute(serde_json::json!({ "pattern": "*.txt", "limit": 2 }), &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert_eq!(result.metadata.get("count"), Some(&serde_json::json!(2)));
        assert_eq!(
            result.metadata.get("truncated"),
            Some(&serde_json::json!(true))
        );
        assert!(result.truncated);
        assert!(
            result
                .output
                .contains("(Results are truncated: showing first 2 results")
        );
        assert_eq!(result.output.lines().count(), 4); // 2 paths + blank + note
        Ok(())
    }

    #[tokio::test]
    async fn glob_sorts_by_modification_time() -> Result<()> {
        use std::fs::FileTimes;
        use std::time::{Duration, SystemTime};

        let dir = tempdir()?;
        let old = dir.path().join("old.txt");
        let new = dir.path().join("new.txt");
        fs::write(&old, "").await?;
        fs::write(&new, "").await?;

        let base = SystemTime::now() - Duration::from_secs(10_000);
        let file = std::fs::File::options().write(true).open(&old)?;
        file.set_times(FileTimes::new().set_modified(base))?;
        drop(file);
        let file = std::fs::File::options().write(true).open(&new)?;
        file.set_times(FileTimes::new().set_modified(base + Duration::from_secs(60)))?;
        drop(file);

        let tool = GlobTool {
            working_directory: dir.path().to_path_buf(),
        };
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(serde_json::json!({ "pattern": "*.txt" }), &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let lines: Vec<&str> = result.output.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("new.txt"), "{}", result.output);
        assert!(lines[1].ends_with("old.txt"), "{}", result.output);
        Ok(())
    }

    #[tokio::test]
    async fn glob_excludes_git_dir_and_hidden_files_unless_requested() -> Result<()> {
        let dir = tempdir()?;
        fs::create_dir(dir.path().join(".git")).await?;
        fs::write(dir.path().join(".git").join("tracked.cfg"), "").await?;
        fs::write(dir.path().join(".hidden.txt"), "").await?;
        fs::write(dir.path().join("visible.txt"), "").await?;

        let tool = GlobTool {
            working_directory: dir.path().to_path_buf(),
        };
        let ctx = ToolContext::new("test", "msg");

        let result = tool
            .execute(serde_json::json!({ "pattern": "**/*.cfg" }), &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert_eq!(result.output, "No files found");

        let result = tool
            .execute(serde_json::json!({ "pattern": "*.txt" }), &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(
            !result.output.contains(".hidden.txt"),
            "hidden file leaked: {}",
            result.output
        );
        assert!(result.output.contains("visible.txt"));
        assert_eq!(result.metadata.get("count"), Some(&serde_json::json!(1)));

        let result = tool
            .execute(
                serde_json::json!({ "pattern": "*.txt", "hidden": true }),
                &ctx,
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(result.output.contains(".hidden.txt"));
        assert_eq!(result.metadata.get("count"), Some(&serde_json::json!(2)));
        Ok(())
    }

    #[tokio::test]
    async fn glob_rejects_file_as_search_path() -> Result<()> {
        let dir = tempdir()?;
        let file = dir.path().join("not_a_dir.txt");
        fs::write(&file, "").await?;

        let tool = GlobTool {
            working_directory: dir.path().to_path_buf(),
        };
        let ctx = ToolContext::new("test", "msg");
        let result = tool
            .execute(
                serde_json::json!({ "pattern": "*.txt", "path": file.to_str().unwrap() }),
                &ctx,
            )
            .await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains("glob path must be a directory"), "{error}");
        Ok(())
    }

    #[tokio::test]
    async fn test_edit_file_tool() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();
        let file_path = dir.path().join("test_edit.txt");
        fs::write(&file_path, "foo bar foo").await?;

        let tool = EditFileTool {
            working_directory: working_dir,
        };

        let args_multiple = serde_json::json!({
            "file_path": file_path.to_str().unwrap(),
            "old_string": "foo",
            "new_string": "baz",
            "replace_all": false
        });
        let ctx = ToolContext::new("test", "msg");
        let result = tool.execute(args_multiple, &ctx).await;
        assert!(result.is_err());

        let args_all = serde_json::json!({
            "file_path": file_path.to_str().unwrap(),
            "old_string": "foo",
            "new_string": "baz",
            "replace_all": true
        });
        let ctx = ToolContext::new("test", "msg");
        let _ = tool
            .execute(args_all, &ctx)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let content = fs::read_to_string(&file_path).await?;
        assert_eq!(content, "baz bar baz");

        Ok(())
    }

    #[tokio::test]
    async fn read_file_without_enforcement_reads_outside_root() -> Result<()> {
        let root = tempdir()?;
        let outside = tempdir()?;
        let file_path = outside.path().join("outside.txt");
        fs::write(&file_path, "outside content").await?;

        let tool = ReadFileTool {
            working_directory: root.path().to_path_buf(),
        };
        let ctx = ToolContext::new("test", "msg").with_enforcement(PathEnforcement::None);
        let result = tool
            .execute(
                serde_json::json!({ "file_path": file_path.to_str().unwrap() }),
                &ctx,
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        assert!(
            result.output.contains("outside content"),
            "{}",
            result.output
        );

        // With containment on, the same request is rejected.
        let ctx = ToolContext::new("test", "msg").with_enforcement(PathEnforcement::Contain);
        let result = tool
            .execute(
                serde_json::json!({ "file_path": file_path.to_str().unwrap() }),
                &ctx,
            )
            .await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains("outside of working directory"), "{error}");
        Ok(())
    }
}
