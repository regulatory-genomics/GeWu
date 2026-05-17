fn strip_tag_blocks(input: &str, tag: &str) -> String {
    let start_tag = format!("<{tag}>");
    let end_tag = format!("</{tag}>");
    let mut output = input.to_string();

    loop {
        let lower = output.to_lowercase();
        let Some(start) = lower.find(&start_tag) else {
            break;
        };
        let Some(end_rel) = lower[start..].find(&end_tag) else {
            break;
        };
        let end = start + end_rel + end_tag.len();
        output.replace_range(start..end, "");
    }

    output
}

pub(crate) fn normalize_assistant_output(input: &str) -> String {
    let cleaned = ["think", "analysis", "reasoning"]
        .into_iter()
        .fold(input.to_string(), |acc, tag| strip_tag_blocks(&acc, tag));
    cleaned.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::normalize_assistant_output;

    #[test]
    fn strips_think_analysis_and_reasoning_blocks() {
        let raw = "<think>internal</think> {\"ok\":true} <analysis>drop</analysis> <reasoning>drop2</reasoning>";
        let cleaned = normalize_assistant_output(raw);
        assert_eq!(cleaned, "{\"ok\":true}");
    }

    #[test]
    fn preserves_regular_text_when_no_tags_present() {
        let raw = "plain response";
        let cleaned = normalize_assistant_output(raw);
        assert_eq!(cleaned, "plain response");
    }
}
