//! Filesystem Tools
//!
//! This module implements tools for interacting with the local filesystem.
//! All file operations are restricted to a configured working directory
//! for security.
//!
//! # Security
//!
//! All file paths are resolved through [`resolve_safe_path`](crate::fs_utils::resolve_safe_path)
//! which prevents directory traversal attacks. Paths cannot escape the
//! configured working directory.
//!
//! # Available Tools
//!
//! - [`ReadFileTool`] - Reads content from files or lists directories
//! - [`WriteFileTool`] - Writes content to files (overwrites existing)
//! - [`EditFileTool`] - Modifies existing files by replacing text
//! - [`GlobTool`] - Finds files using glob patterns, respects .gitignore

use crate::fs_utils::{
    PathPermissionDecision, classify_path_permission, configured_read_roots, glob_with_gitignore,
    into_tool_error, is_binary_file, is_read_allowed, mime_type, permission_required_result,
    read_bytes, read_text, resolve_read_path, write,
};
use crate::replacers::smart_replace;
use anyhow::Result;
use async_trait::async_trait;
use gewu_core::permission::PermissionAction;
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

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ReadArgs {
    #[serde(alias = "filePath")]
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
    #[serde(alias = "filePath")]
    pub file_path: String,
    pub content: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct EditArgs {
    #[serde(alias = "filePath")]
    pub file_path: String,
    #[serde(alias = "oldString")]
    pub old_string: String,
    #[serde(alias = "newString")]
    pub new_string: String,
    #[serde(alias = "replaceAll")]
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GlobArgs {
    pub pattern: String,
    pub path: Option<String>,
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

        let file_path = match classify_path_permission(
            PermissionAction::Read,
            &path_from_tool_input(&self.working_directory, &input.file_path),
            &configured_read_roots(&self.working_directory),
            &ctx.permission_grants(),
            "read_file",
            &ctx.session_id,
            false,
        )
        .map_err(into_tool_error)?
        {
            PathPermissionDecision::Allowed { canonical_path } => canonical_path,
            PathPermissionDecision::Requires(request) => {
                return Ok(ToolResult::new(
                    "read_file",
                    permission_required_result(request).to_string(),
                ));
            }
            PathPermissionDecision::Denied { reason } => return Err(into_tool_error(anyhow::anyhow!(reason))),
        };

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

            let truncated_line = if text.len() > MAX_LINE_LENGTH {
                let boundary = floor_char_boundary(&text, MAX_LINE_LENGTH);
                format!("{}{}", &text[..boundary], MAX_LINE_SUFFIX)
            } else {
                text.clone()
            };

            let line_size = truncated_line.len() + 1;
            if bytes + line_size > MAX_BYTES {
                truncated_by_bytes = true;
                has_more_lines = true;
                break;
            }

            lines.push(truncated_line);
            bytes += line_size;
        }

        if total_lines < input.offset && !(total_lines == 0 && input.offset == 1) {
            return Ok(ToolResult::new(
                "read_file",
                format!(
                    "Offset {} is out of range for this file ({} lines)",
                    input.offset, total_lines
                ),
            ));
        }

        let content: Vec<String> = lines
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}: {}", start + i + 1, line))
            .collect();

        let total_lines_count = total_lines;
        let last_read_line = input.offset + lines.len() - 1;
        let next_offset = last_read_line + 1;
        let has_more_lines = has_more_lines || truncated_by_bytes;

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
                input.offset, last_read_line, total_lines_count, next_offset
            ));
        } else {
            output.push_str(&format!(
                "\n\n(End of file - total {} lines)",
                total_lines_count
            ));
        }

        Ok(ToolResult::new("read_file", output))
    }
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let mut boundary = index;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
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

        let file_path = match classify_path_permission(
            PermissionAction::Write,
            &path_from_tool_input(&self.working_directory, &input.file_path),
            &[self.working_directory.clone()],
            &ctx.permission_grants(),
            "write_file",
            &ctx.session_id,
            true,
        )
        .map_err(into_tool_error)?
        {
            PathPermissionDecision::Allowed { canonical_path } => canonical_path,
            PathPermissionDecision::Requires(request) => {
                return Ok(ToolResult::new(
                    "write_file",
                    permission_required_result(request).to_string(),
                ));
            }
            PathPermissionDecision::Denied { reason } => return Err(into_tool_error(anyhow::anyhow!(reason))),
        };

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

        let file_path = match classify_path_permission(
            PermissionAction::Write,
            &path_from_tool_input(&self.working_directory, &input.file_path),
            &[self.working_directory.clone()],
            &ctx.permission_grants(),
            "edit_file",
            &ctx.session_id,
            true,
        )
        .map_err(into_tool_error)?
        {
            PathPermissionDecision::Allowed { canonical_path } => canonical_path,
            PathPermissionDecision::Requires(request) => {
                return Ok(ToolResult::new(
                    "edit_file",
                    permission_required_result(request).to_string(),
                ));
            }
            PathPermissionDecision::Denied { reason } => return Err(into_tool_error(anyhow::anyhow!(reason))),
        };

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

fn path_from_tool_input(working_directory: &std::path::Path, input_path: &str) -> PathBuf {
    let path = std::path::Path::new(input_path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_directory.join(path)
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
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: GlobArgs = serde_json::from_value(args).map_err(into_tool_error)?;

        let search_dir = if let Some(p) = &input.path {
            resolve_read_path(&self.working_directory, p).map_err(into_tool_error)?
        } else {
            self.working_directory.clone()
        };

        let paths = glob_with_gitignore(&search_dir, &input.pattern).map_err(into_tool_error)?;

        let paths_str: Vec<String> = paths
            .into_iter()
            .filter(|p| is_read_allowed(&self.working_directory, p))
            .map(|p| p.to_string_lossy().into_owned())
            .collect();

        Ok(ToolResult::new(
            "glob",
            serde_json::to_string_pretty(&paths_str).unwrap(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
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
    async fn test_read_file_tool_truncates_long_utf8_lines_on_char_boundary() -> Result<()> {
        let dir = tempdir()?;
        let working_dir = dir.path().to_path_buf();
        let file_path = dir.path().join("long_utf8.txt");
        let long_line = format!("{}中", "a".repeat(MAX_LINE_LENGTH - 1));
        fs::write(&file_path, long_line).await?;

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

        assert!(result.output.contains(MAX_LINE_SUFFIX));
        assert!(result.output.contains(&"a".repeat(MAX_LINE_LENGTH - 1)));
        assert!(!result.output.contains('中'));
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
        let files: Vec<String> = serde_json::from_str(&result.output)?;

        assert_eq!(files.len(), 2);
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
}
