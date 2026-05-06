use std::path::Path;

use crate::inspector::policy::ExposureLevel;

pub fn redact_path(raw: &str, level: ExposureLevel) -> String {
    if level.allows_source_paths() {
        return raw.to_string();
    }
    let path = Path::new(raw);
    if let Some(file_name) = path.file_name() {
        format!("…/{}", file_name.to_string_lossy())
    } else {
        "…".to_string()
    }
}

pub fn redact_prompt(raw: &str, level: ExposureLevel) -> String {
    if level.allows_full_prompts() {
        return raw.to_string();
    }
    let preview_len = 120.min(raw.len());
    let preview: String = raw.chars().take(preview_len).collect();
    if raw.len() > preview_len {
        format!("{preview}…")
    } else {
        preview
    }
}

pub fn redact_output(raw: &str, level: ExposureLevel) -> String {
    if level.allows_full_output() {
        return raw.to_string();
    }
    let limit = 200;
    let preview: String = raw.chars().take(limit).collect();
    if raw.len() > limit {
        format!("{preview}… [truncated]")
    } else {
        preview
    }
}

pub fn redact_error(raw: &str, level: ExposureLevel) -> String {
    if level.allows_error_details() {
        return raw.to_string();
    }
    let limit = 160;
    let preview: String = raw.chars().take(limit).collect();
    if raw.len() > limit {
        format!("{preview}…")
    } else {
        preview
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspector::policy::ExposureLevel;

    #[test]
    fn path_redacted_at_public() {
        let result = redact_path(
            "/home/user/.gewu/specialists/foo.yaml",
            ExposureLevel::PublicSummary,
        );
        assert_eq!(result, "…/foo.yaml");
    }

    #[test]
    fn path_visible_at_local_debug() {
        let raw = "/home/user/.gewu/specialists/foo.yaml";
        assert_eq!(redact_path(raw, ExposureLevel::LocalDebug), raw);
    }

    #[test]
    fn prompt_truncated_at_public() {
        let long = "a".repeat(200);
        let result = redact_prompt(&long, ExposureLevel::PublicSummary);
        assert!(result.len() < 200);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn prompt_full_at_raw() {
        let long = "a".repeat(200);
        assert_eq!(redact_prompt(&long, ExposureLevel::RawInternal), long);
    }

    #[test]
    fn output_truncated_at_public() {
        let long = "x".repeat(500);
        let result = redact_output(&long, ExposureLevel::PublicSummary);
        assert!(result.contains("[truncated]"));
    }

    #[test]
    fn output_full_at_local_debug() {
        let long = "x".repeat(500);
        assert_eq!(redact_output(&long, ExposureLevel::LocalDebug), long);
    }

    #[test]
    fn short_strings_not_truncated() {
        let short = "hello";
        assert_eq!(redact_prompt(short, ExposureLevel::PublicSummary), short);
        assert_eq!(redact_output(short, ExposureLevel::PublicSummary), short);
        assert_eq!(redact_error(short, ExposureLevel::PublicSummary), short);
    }
}
