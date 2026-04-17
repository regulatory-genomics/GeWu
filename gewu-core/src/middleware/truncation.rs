use crate::types::TruncationResult;
use std::path::PathBuf;

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;
pub const TRUNCATION_DIR: &str = ".gewu/truncation";

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
    pub fn new() -> Self {
        Self {
            max_lines: MAX_LINES,
            max_bytes: MAX_BYTES,
            truncation_dir: PathBuf::from(TRUNCATION_DIR),
        }
    }

    pub fn with_dir(mut self, dir: PathBuf) -> Self {
        self.truncation_dir = dir;
        self
    }

    pub fn with_limits(mut self, max_lines: usize, max_bytes: usize) -> Self {
        self.max_lines = max_lines;
        self.max_bytes = max_bytes;
        self
    }

    pub fn truncate(&self, text: &str, _has_task_tool: bool) -> TruncationResult {
        let lines: Vec<&str> = text.lines().collect();
        let total_bytes = text.len();
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

        for i in 0..lines.len().min(self.max_lines) {
            let line_size = lines[i].len() + if i > 0 { 1 } else { 0 };
            if bytes + line_size > self.max_bytes {
                hit_bytes = true;
                break;
            }
            out_lines.push(lines[i]);
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

        let truncated_content = if hit_bytes {
            format!(
                "{}\n\n...{} {} truncated...\n\n{}",
                preview, removed, unit, hint
            )
        } else {
            format!(
                "{}\n\n...{} {} truncated...\n\n{}",
                preview, removed, unit, hint
            )
        };

        if let Err(e) = std::fs::create_dir_all(&self.truncation_dir) {
            tracing::warn!("Failed to create truncation dir: {}", e);
        }
        if let Err(e) = std::fs::write(&file_path, text) {
            tracing::warn!("Failed to write truncation file: {}", e);
        }

        TruncationResult {
            content: truncated_content,
            truncated: true,
            output_path: Some(file_path),
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
        let result = service.truncate(text, false);
        assert!(!result.truncated);
        assert_eq!(result.content, text);
        assert!(result.output_path.is_none());
    }

    #[test]
    fn test_truncation_by_lines() {
        let service = TruncationService::new();
        let text: String = (0..3000).map(|i| format!("line {}\n", i)).collect();
        let result = service.truncate(&text, false);
        assert!(result.truncated);
        assert!(result.output_path.is_some());
        assert!(result.content.contains("truncated"));
    }

    #[test]
    fn test_truncation_by_bytes() {
        let service = TruncationService::new();
        let line = "x".repeat(100);
        let text: String = (0..600).map(|_| format!("{}\n", line)).collect();
        let result = service.truncate(&text, false);
        assert!(result.truncated);
        assert!(result.output_path.is_some());
    }
}
