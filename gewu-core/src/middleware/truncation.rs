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

fn truncate_json_value(val: &serde_json::Value, budget: usize) -> serde_json::Value {
    match val {
        serde_json::Value::Array(arr) => {
            let mut result = Vec::new();
            let mut used = 2;
            for item in arr {
                let item_str = serde_json::to_string(item).unwrap_or_default();
                let cost = item_str.len() + if result.is_empty() { 0 } else { 1 };
                if used + cost > budget {
                    break;
                }
                used += cost;
                result.push(item.clone());
            }
            serde_json::Value::Array(result)
        }
        serde_json::Value::Object(map) => {
            let mut result = serde_json::Map::new();
            let mut used = 2;
            for (k, v) in map {
                let v_budget = budget.saturating_sub(used).saturating_sub(k.len() + 4);
                let truncated_v = truncate_json_value(v, v_budget);
                let serialized = serde_json::to_string(&truncated_v).unwrap_or_default();
                let entry_str = format!("\"{k}\":{serialized}");
                let cost = entry_str.len() + if result.is_empty() { 0 } else { 1 };
                if used + cost > budget {
                    break;
                }
                used += cost;
                result.insert(k.clone(), truncated_v);
            }
            serde_json::Value::Object(result)
        }
        other => other.clone(),
    }
}

impl Default for TruncationService {
    fn default() -> Self {
        Self::new()
    }
}

impl TruncationService {
    pub fn new() -> Self {
        let truncation_dir = dirs::home_dir()
            .map(|h| h.join(".gewu").join("truncation"))
            .unwrap_or_else(|| PathBuf::from(TRUNCATION_DIR));
        Self {
            max_lines: MAX_LINES,
            max_bytes: MAX_BYTES,
            truncation_dir,
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

        let preview = if let Some(smart) = self.try_json_truncate(text) {
            smart
        } else {
            self.line_truncate(&lines)
        };

        let file_name = format!("tool_{}.txt", uuid::Uuid::new_v4());
        let file_path = self.truncation_dir.join(&file_name);

        let hint = format!(
            "The tool call succeeded but the output was truncated ({} → {} bytes). \
            Full output saved to the file above. \
            Use grep to search the full content or read with offset/limit to view specific sections.",
            total_bytes,
            preview.len()
        );

        let truncated_content = format!("{preview}\n\n{hint}");

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

    fn line_truncate(&self, lines: &[&str]) -> String {
        let mut out_lines: Vec<&str> = Vec::new();
        let mut bytes = 0;

        for (i, line) in lines.iter().take(self.max_lines).enumerate() {
            let line_size = line.len() + if i > 0 { 1 } else { 0 };
            if bytes + line_size > self.max_bytes {
                break;
            }
            out_lines.push(line);
            bytes += line_size;
        }

        out_lines.join("\n")
    }

    /// For JSON responses containing arrays, truncate at complete array element
    /// boundaries so the LLM receives valid, parseable JSON.
    fn try_json_truncate(&self, text: &str) -> Option<String> {
        let trimmed = text.trim();
        if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
            return None;
        }

        let val: serde_json::Value = serde_json::from_str(trimmed).ok()?;

        let truncated = truncate_json_value(&val, self.max_bytes);
        let serialized = serde_json::to_string(&truncated).ok()?;

        Some(serialized)
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
        let text: String = (0..3000).map(|i| format!("line {i}\n")).collect();
        let result = service.truncate(&text, false);
        assert!(result.truncated);
        assert!(result.output_path.is_some());
        assert!(result.content.contains("truncated"));
    }

    #[test]
    fn test_truncation_by_bytes() {
        let service = TruncationService::new();
        let line = "x".repeat(100);
        let text: String = (0..600).map(|_| format!("{line}\n")).collect();
        let result = service.truncate(&text, false);
        assert!(result.truncated);
        assert!(result.output_path.is_some());
    }

    #[test]
    fn test_json_array_truncation() {
        let service = TruncationService::new().with_limits(2000, 200);
        let arr: Vec<serde_json::Value> = (0..100)
            .map(|i| serde_json::json!({"id": i, "name": format!("item_{}", i)}))
            .collect();
        let text = serde_json::to_string(&arr).unwrap();
        let result = service.truncate(&text, false);
        assert!(result.truncated);
        // The truncated content should be valid JSON
        let preview = result.content.split("\n\nThe tool call").next().unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(preview).unwrap();
        assert!(!parsed.is_empty());
        assert!(parsed.len() < 100);
    }

    #[test]
    fn test_json_object_with_array_truncation() {
        let service = TruncationService::new().with_limits(2000, 500);
        let obj = serde_json::json!({
            "cached": true,
            "rows": (0..200).map(|i| serde_json::json!({"term": format!("GO:{:07}", i), "fe": 2.5})).collect::<Vec<_>>()
        });
        let text = serde_json::to_string(&obj).unwrap();
        let result = service.truncate(&text, false);
        assert!(result.truncated);
        let preview = result.content.split("\n\nThe tool call").next().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(preview).unwrap();
        assert!(parsed["cached"].as_bool().unwrap());
        assert!(parsed["rows"].as_array().unwrap().len() < 200);
    }
}
