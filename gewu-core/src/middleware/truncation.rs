//! Middleware services for agent processing.
//!
//! This module provides middleware components that intercept and process
//! agent operations, such as output truncation for managing response sizes.

use crate::types::TruncationResult;
use std::path::PathBuf;

/// Maximum number of lines to retain before truncation.
pub const MAX_LINES: usize = 2000;

/// Maximum bytes to retain before truncation.
pub const MAX_BYTES: usize = 50 * 1024;

/// Directory name for storing truncated output files.
pub const TRUNCATION_DIR: &str = "truncation";

/// Service for truncating long tool outputs.
///
/// The `TruncationService` limits the size of tool outputs by truncating
/// them to a maximum number of lines or bytes. When output is truncated,
/// the full content is saved to a file and a placeholder message is
/// returned instead.
#[derive(Clone)]
pub struct TruncationService {
    max_lines: usize,
    max_bytes: usize,
    truncation_dir: PathBuf,
}

impl Default for TruncationService {
    fn default() -> Self {
        Self::new()
    }
}

impl TruncationService {
    /// Creates a new `TruncationService` with default limits.
    ///
    /// Default limits are 2000 lines or 50KB, whichever is reached first.
    pub fn new() -> Self {
        Self {
            max_lines: MAX_LINES,
            max_bytes: MAX_BYTES,
            truncation_dir: PathBuf::from(TRUNCATION_DIR),
        }
    }

    /// Sets a custom directory for storing truncated files.
    pub fn with_dir(mut self, dir: PathBuf) -> Self {
        self.truncation_dir = dir;
        self
    }

    /// Sets custom limits for truncation.
    ///
    /// # Arguments
    ///
    /// * `max_lines` - Maximum lines to retain
    /// * `max_bytes` - Maximum bytes to retain
    pub fn with_limits(mut self, max_lines: usize, max_bytes: usize) -> Self {
        self.max_lines = max_lines;
        self.max_bytes = max_bytes;
        self
    }

    /// Truncates text if it exceeds the configured limits.
    ///
    /// If the text exceeds either the line or byte limit, it is truncated
    /// and the full content is saved to a file. The returned `TruncationResult`
    /// contains either the original text (if within limits) or a truncated
    /// version with a reference to the saved file.
    ///
    /// # Arguments
    ///
    /// * `text` - The text to potentially truncate
    ///
    /// # Returns
    ///
    /// A [`TruncationResult`] containing either the original or truncated content
    pub fn truncate(&self, text: &str) -> TruncationResult {
        // Measure against the body without the trailing newline: otherwise a
        // text over the byte limit only by its final newline triggers a
        // "...1 bytes truncated..." notice that removes nothing meaningful.
        let body = text.strip_suffix('\n').unwrap_or(text);
        let lines: Vec<&str> = body.lines().collect();
        let total_bytes = body.len();
        let total_lines = lines.len();

        if total_lines <= self.max_lines && total_bytes <= self.max_bytes {
            return TruncationResult {
                content: text.to_string(),
                truncated: false,
                output_path: None,
            };
        }

        let mut out_lines: Vec<&str> = Vec::new();
        let mut bytes = 0;
        let mut hit_bytes = false;

        for (i, line) in lines.iter().enumerate().take(lines.len().min(self.max_lines)) {
            let line_size = line.len() + if i > 0 { 1 } else { 0 };
            if bytes + line_size > self.max_bytes {
                hit_bytes = true;
                break;
            }
            out_lines.push(line);
            bytes += line_size;
        }

        let removed = if hit_bytes {
            total_bytes - bytes
        } else {
            total_lines - out_lines.len()
        };
        let unit = if hit_bytes { "bytes" } else { "lines" };

        let preview = out_lines.join("\n");
        let file_name = format!("tool_{}.txt", uuid::Uuid::new_v4());
        let file_path = self.truncation_dir.join(&file_name);

        let hint = "The tool call succeeded but the output was truncated. \
            Full output saved to the file above. \
            Use grep to search the full content or read with offset/limit to view specific sections.";

        let truncated_content = format!(
            "{}\n\n...{} {} truncated...\n\n{}",
            preview, removed, unit, hint
        );

        let dir_created = std::fs::create_dir_all(&self.truncation_dir).is_ok();
        let write_ok = dir_created && std::fs::write(&file_path, text).is_ok();
        if !write_ok {
            tracing::warn!("Failed to write truncation file: {}", file_path.display());
        }

        TruncationResult {
            content: truncated_content,
            truncated: true,
            // Only point the model at the file when it actually exists.
            output_path: write_ok.then_some(file_path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_truncation_small_output() {
        let service = TruncationService::new();
        let text = "Hello, world!";
        let result = service.truncate(text);
        assert!(!result.truncated);
        assert_eq!(result.content, text);
        assert!(result.output_path.is_none());
    }

    #[test]
    fn test_truncation_by_lines() {
        let service = TruncationService::new();
        let text: String = (0..3000).map(|i| format!("line {}\n", i)).collect();
        let result = service.truncate(&text);
        assert!(result.truncated);
        assert!(result.output_path.is_some());
        assert!(result.content.contains("truncated"));
    }

    #[test]
    fn test_truncation_by_bytes() {
        let service = TruncationService::new();
        let line = "x".repeat(100);
        let text: String = (0..600).map(|_| format!("{}\n", line)).collect();
        let result = service.truncate(&text);
        assert!(result.truncated);
        assert!(result.output_path.is_some());
    }
}
