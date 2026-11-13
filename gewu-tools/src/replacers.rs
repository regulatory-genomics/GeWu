const SINGLE_CANDIDATE_SIMILARITY_THRESHOLD: f64 = 0.3;
const MULTIPLE_CANDIDATES_SIMILARITY_THRESHOLD: f64 = 0.3;

/// Slices `line` from byte index `idx`, backing up to the nearest UTF-8 char
/// boundary so multi-byte input never panics mid-character.
fn slice_from_char_boundary(line: &str, idx: usize) -> &str {
    if idx >= line.len() {
        return "";
    }
    let mut idx = idx;
    while idx > 0 && !line.is_char_boundary(idx) {
        idx -= 1;
    }
    &line[idx..]
}

fn levenshtein(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return (a.len().max(b.len())) as f64;
    }

    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let a_len = a_chars.len();
    let b_len = b_chars.len();

    let mut matrix = vec![vec![0usize; b_len + 1]; a_len + 1];

    for (i, row) in matrix.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in matrix[0].iter_mut().enumerate() {
        *cell = j;
    }

    for i in 1..=a_len {
        for j in 1..=b_len {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            matrix[i][j] = (matrix[i - 1][j] + 1)
                .min(matrix[i][j - 1] + 1)
                .min(matrix[i - 1][j - 1] + cost);
        }
    }

    matrix[a_len][b_len] as f64
}

fn simple_replace(content: &str, search: &str) -> Option<(usize, usize)> {
    if !content.contains(search) {
        return None;
    }
    let idx = content.find(search)?;
    Some((idx, idx + search.len()))
}

fn line_trimmed_replace(content: &str, search: &str) -> Option<(usize, usize)> {
    let original_lines: Vec<&str> = content.split('\n').collect();
    let mut search_lines: Vec<&str> = search.split('\n').collect();

    if search_lines.last() == Some(&"") {
        search_lines.pop();
    }

    if search_lines.is_empty() || search_lines.len() > original_lines.len() {
        return None;
    }

    for i in 0..=(original_lines.len() - search_lines.len()) {
        let mut matches_line = true;

        for j in 0..search_lines.len() {
            let original_trimmed = original_lines[i + j].trim();
            let search_trimmed = search_lines[j].trim();

            if original_trimmed != search_trimmed {
                matches_line = false;
                break;
            }
        }

        if matches_line {
            let match_start_index: usize =
                original_lines[..i].iter().map(|line| line.len() + 1).sum();

            let mut match_end_index = match_start_index;
            for k in 0..search_lines.len() {
                match_end_index += original_lines[i + k].len();
                if k < search_lines.len() - 1 {
                    match_end_index += 1;
                }
            }

            return Some((match_start_index, match_end_index));
        }
    }

    None
}

fn remove_indentation(text: &str) -> (String, usize) {
    let lines: Vec<&str> = text.split('\n').collect();
    let non_empty_lines: Vec<&str> = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect();

    if non_empty_lines.is_empty() {
        return (text.to_string(), 0);
    }

    let min_indent = non_empty_lines
        .iter()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            if trimmed.is_empty() {
                None
            } else {
                Some(line.len() - trimmed.len())
            }
        })
        .min()
        .unwrap_or(0);

    let dedented: String = lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                line.to_string()
            } else {
                slice_from_char_boundary(line, min_indent).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    (dedented, min_indent)
}

fn block_anchor_replace(content: &str, search: &str) -> Option<(usize, usize)> {
    let original_lines: Vec<&str> = content.split('\n').collect();
    let mut search_lines: Vec<&str> = search.split('\n').collect();

    if search_lines.len() < 3 {
        return None;
    }

    if search_lines.last() == Some(&"") {
        search_lines.pop();
    }

    let first_line_search = search_lines[0].trim();
    let last_line_search = search_lines[search_lines.len() - 1].trim();
    let search_block_size = search_lines.len();

    #[derive(Clone)]
    struct Candidate {
        start_line: usize,
        end_line: usize,
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    for i in 0..original_lines.len() {
        if original_lines[i].trim() != first_line_search {
            continue;
        }

        for (j, line) in original_lines.iter().enumerate().skip(i + 2) {
            if line.trim() == last_line_search {
                candidates.push(Candidate {
                    start_line: i,
                    end_line: j,
                });
                break;
            }
        }
    }

    if candidates.is_empty() {
        return None;
    }

    if candidates.len() == 1 {
        let cand = &candidates[0];
        let actual_block_size = cand.end_line - cand.start_line + 1;

        let lines_to_check = (search_block_size - 2).min(actual_block_size.saturating_sub(2));

        let similarity = if lines_to_check > 0 {
            // Average similarity across ALL middle lines of the candidate
            // block, then gate on the threshold. Checking a single line and
            // breaking early would make the gate a no-op (any two non-empty
            // lines have Levenshtein similarity > 0), allowing wildly
            // different blocks to match and corrupting the file.
            let mut total = 0.0;
            for (j, raw_search_line) in search_lines
                .iter()
                .enumerate()
                .take(search_block_size - 1)
                .skip(1)
            {
                let orig_idx = cand.start_line + j;
                if orig_idx >= original_lines.len() {
                    break;
                }
                let original_line = original_lines[orig_idx].trim();
                let search_line = raw_search_line.trim();
                let max_len = original_line.len().max(search_line.len());
                if max_len == 0 {
                    continue;
                }
                let distance = levenshtein(original_line, search_line);
                total += 1.0 - distance / max_len as f64;
            }
            total / lines_to_check as f64
        } else {
            1.0
        };

        if similarity >= SINGLE_CANDIDATE_SIMILARITY_THRESHOLD {
            let match_start_index: usize = original_lines[..cand.start_line]
                .iter()
                .map(|line| line.len() + 1)
                .sum();
            let mut match_end_index = match_start_index;
            for (k, line) in original_lines
                .iter()
                .enumerate()
                .take(cand.end_line + 1)
                .skip(cand.start_line)
            {
                match_end_index += line.len();
                if k < cand.end_line {
                    match_end_index += 1;
                }
            }
            return Some((match_start_index, match_end_index));
        }
        return None;
    }

    let mut best_match: Option<Candidate> = None;
    let mut max_similarity = -1.0;

    for cand in &candidates {
        let actual_block_size = cand.end_line - cand.start_line + 1;
        let mut similarity = 0.0;
        let lines_to_check = (search_block_size - 2).min(actual_block_size.saturating_sub(2));

        if lines_to_check > 0 {
            for (j, raw_search_line) in search_lines
                .iter()
                .enumerate()
                .take(search_block_size - 1)
                .skip(1)
            {
                let orig_idx = cand.start_line + j;
                if orig_idx >= original_lines.len() {
                    continue;
                }
                let original_line = original_lines[orig_idx].trim();
                let search_line = raw_search_line.trim();
                let max_len = original_line.len().max(search_line.len());
                if max_len == 0 {
                    continue;
                }
                let distance = levenshtein(original_line, search_line);
                similarity += 1.0 - distance / max_len as f64;
            }
            similarity /= lines_to_check as f64;
        } else {
            similarity = 1.0;
        }

        if similarity > max_similarity {
            max_similarity = similarity;
            best_match = Some(cand.clone());
        }
    }

    if max_similarity >= MULTIPLE_CANDIDATES_SIMILARITY_THRESHOLD
        && let Some(best) = best_match
    {
        let match_start_index: usize = original_lines[..best.start_line]
            .iter()
            .map(|line| line.len() + 1)
            .sum();
        let mut match_end_index = match_start_index;
        for (k, line) in original_lines
            .iter()
            .enumerate()
            .take(best.end_line + 1)
            .skip(best.start_line)
        {
            match_end_index += line.len();
            if k < best.end_line {
                match_end_index += 1;
            }
        }
        return Some((match_start_index, match_end_index));
    }

    None
}

fn whitespace_normalized_replace(content: &str, search: &str) -> Option<(usize, usize)> {
    let normalize = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let normalized_search = normalize(search);
    let lines: Vec<&str> = content.split('\n').collect();

    for (i, line) in lines.iter().enumerate() {
        let normalized_line = normalize(line);

        if normalized_line == normalized_search {
            let start: usize = lines[..i].iter().map(|line| line.len() + 1).sum();
            return Some((start, start + line.len()));
        }
    }

    None
}

fn indentation_flexible_replace(content: &str, search: &str) -> Option<(usize, usize)> {
    let content_lines: Vec<&str> = content.split('\n').collect();
    let find_lines: Vec<&str> = search.split('\n').collect();

    if find_lines.is_empty() || find_lines.len() > content_lines.len() {
        return None;
    }

    let (normalized_find, _) = remove_indentation(search);

    for i in 0..=(content_lines.len() - find_lines.len()) {
        let block = content_lines[i..i + find_lines.len()].join("\n");
        let (normalized_block, _) = remove_indentation(&block);
        if normalized_block == normalized_find {
            let start: usize = content_lines[..i].iter().map(|line| line.len() + 1).sum();
            let end = start + block.len();
            return Some((start, end));
        }
    }

    None
}

fn unescape_string(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                chars.next();
                match next {
                    'n' => result.push('\n'),
                    't' => result.push('\t'),
                    'r' => result.push('\r'),
                    '\'' => result.push('\''),
                    '"' => result.push('"'),
                    '`' => result.push('`'),
                    '\\' => result.push('\\'),
                    '$' => result.push('$'),
                    _ => {
                        result.push('\\');
                        result.push(next);
                    }
                }
            } else {
                result.push(c);
            }
        } else {
            result.push(c);
        }
    }

    result
}

fn escape_normalized_replace(content: &str, search: &str) -> Option<(usize, usize)> {
    let unescaped_search = unescape_string(search);

    if !content.contains(&unescaped_search) {
        return None;
    }

    let idx = content.find(&unescaped_search)?;

    Some((idx, idx + unescaped_search.len()))
}

fn trimmed_boundary_replace(content: &str, search: &str) -> Option<(usize, usize)> {
    let trimmed_search = search.trim();

    if trimmed_search == search {
        return None;
    }

    if !content.contains(trimmed_search) {
        return None;
    }

    let idx = content.find(trimmed_search)?;
    let last_idx = content.rfind(trimmed_search)?;

    if idx != last_idx {
        return None;
    }

    Some((idx, idx + trimmed_search.len()))
}

fn context_aware_replace(content: &str, search: &str) -> Option<(usize, usize)> {
    let content_lines: Vec<&str> = content.split('\n').collect();
    let mut search_lines: Vec<&str> = search.split('\n').collect();

    if search_lines.len() < 3 {
        return None;
    }

    if search_lines.last() == Some(&"") {
        search_lines.pop();
    }

    let first_line = search_lines[0].trim();
    let last_line = search_lines[search_lines.len() - 1].trim();

    for i in 0..content_lines.len() {
        if content_lines[i].trim() != first_line {
            continue;
        }

        for j in (i + 2)..content_lines.len() {
            if content_lines[j].trim() == last_line {
                let block_lines: Vec<&str> = content_lines[i..=j].to_vec();
                let block = block_lines.join("\n");

                if block_lines.len() == search_lines.len() {
                    let mut matching_lines = 0;
                    let mut total_non_empty = 0;

                    for k in 1..block_lines.len() - 1 {
                        let block_line = block_lines[k].trim();
                        let search_line = search_lines[k].trim();

                        if !block_line.is_empty() || !search_line.is_empty() {
                            total_non_empty += 1;
                            if block_line == search_line {
                                matching_lines += 1;
                            }
                        }
                    }

                    let ratio = if total_non_empty > 0 {
                        matching_lines as f64 / total_non_empty as f64
                    } else {
                        0.0
                    };

                    if ratio >= 0.5 {
                        let start: usize =
                            content_lines[..i].iter().map(|line| line.len() + 1).sum();
                        let end = start + block.len();
                        return Some((start, end));
                    }
                }
                break;
            }
        }
    }

    None
}

fn multi_occurrence_replace(content: &str, search: &str) -> Option<(usize, usize)> {
    if !content.contains(search) {
        return None;
    }
    let idx = content.find(search)?;
    Some((idx, idx + search.len()))
}

pub fn smart_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<String, String> {
    if old_string == new_string {
        return Err("No changes to apply: oldString and newString are identical.".to_string());
    }

    let first_match = simple_replace(content, old_string);
    let occurrence_count = content.matches(old_string).count();

    if !replace_all {
        if let Some((start, end)) = first_match {
            if occurrence_count > 1 {
                return Err("Found multiple matches for oldString. Provide more surrounding context to make the match unique.".to_string());
            }
            let prefix = &content[..start];
            let suffix = &content[end..];
            return Ok(format!("{}{}{}", prefix, new_string, suffix));
        }
    } else {
        if occurrence_count > 0 {
            return Ok(content.replace(old_string, new_string));
        }
    }

    for strategy in &[
        line_trimmed_replace,
        block_anchor_replace,
        whitespace_normalized_replace,
        indentation_flexible_replace,
        escape_normalized_replace,
        trimmed_boundary_replace,
        context_aware_replace,
        multi_occurrence_replace,
    ] {
        if let Some((start, end)) = strategy(content, old_string) {
            // Reaching the strategy loop with `replace_all` implies zero exact
            // occurrences, so `content.replace` would be a silent no-op
            // reporting success without changing anything. Apply the single
            // fuzzy span instead so the edit actually happens.
            let prefix = &content[..start];
            let suffix = &content[end..];
            return Ok(format!("{}{}{}", prefix, new_string, suffix));
        }
    }

    Err("oldString not found in content".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_replace() {
        let content = "hello world";
        let result = smart_replace(content, "hello", "goodbye", false);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "goodbye world");
    }

    #[test]
    fn test_simple_replace_all() {
        let content = "foo bar foo";
        let result = smart_replace(content, "foo", "baz", true);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "baz bar baz");
    }

    #[test]
    fn test_simple_replace_not_found() {
        let content = "foo bar";
        let result = smart_replace(content, "baz", "qux", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("oldString not found"));
    }

    #[test]
    fn test_line_trimmed_replace() {
        let content = "  hello  \n  world  ";
        let result = smart_replace(content, "hello\nworld", "hi\nworld", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_whitespace_normalized_replace() {
        let content = "foo   bar    baz";
        let result = smart_replace(content, "foo   bar", "hello world", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_escape_normalized_replace() {
        let content = "hello\\nworld";
        let result = smart_replace(content, "hello\\nworld", "hi\nworld", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_trimmed_boundary_replace() {
        let content = "   foo   \n   bar   ";
        let result = smart_replace(content, "   foo   ", "new foo", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_levenshtein() {
        assert_eq!(levenshtein("hello", "hello"), 0.0);
        assert_eq!(levenshtein("hello", "hallo"), 1.0);
        assert_eq!(levenshtein("kitten", "sitting"), 3.0);
    }

    #[test]
    fn test_block_anchor_replace() {
        let content = "fn foo() {\n    let x = 1;\n    let y = 2;\n}\nfn bar() {}";
        let search = "fn foo() {\n    let x = 1;\n    let y = 2;\n}";
        let result = smart_replace(
            content,
            search,
            "fn foo() {\n    let x = 10;\n    let y = 20;\n}",
            false,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_indentation_flexible_replace() {
        let content = "    if true {\n        let x = 1;\n    }";
        let search = "if true {\n    let x = 1;\n}";
        let result = smart_replace(content, search, "if false {\n    let x = 2;\n}", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_context_aware_replace() {
        let content = "fn foo() {\n    let x = 1;\n    let y = 2;\n}\nfn bar() {}";
        let search = "fn foo() {\n    let x = 1;\n    let y = 2;\n}";
        let result = smart_replace(
            content,
            search,
            "fn foo() {\n    let x = 10;\n    let y = 20;\n}",
            false,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_block_anchor_single_candidate_rejects_dissimilar_block() {
        // The anchors match but the middle of the candidate block is entirely
        // different from the search — the fuzzy gate must reject it instead of
        // silently replacing the whole 30-line span.
        let mut content = String::from("fn config() {\n");
        for i in 0..30 {
            content.push_str(&format!("    real_line_{}();\n", i));
        }
        content.push_str("}\n");
        content.push_str("fn other() {}\n");

        let search = "fn config() {\nplaceholder\n}";
        let result = smart_replace(&content, search, "fn config() {\n}\n", false);
        assert!(result.is_err(), "dissimilar block must not match");
    }

    #[test]
    fn test_block_anchor_single_candidate_accepts_similar_block() {
        // Small typo in one middle line: fuzzy match should still succeed.
        let content = "fn foo() {\n    let x = 1;\n    let y = 2;\n}\nfn bar() {}";
        let search = "fn foo() {\n    let x = 1;\n    let y = 3;\n}";
        let result = smart_replace(content, search, "fn foo() {\n    let x = 10;\n}", false);
        assert!(result.is_ok());
        let replaced = result.unwrap();
        assert!(replaced.contains("let x = 10;"));
        assert!(!replaced.contains("let y = 2;"));
    }

    #[test]
    fn test_search_longer_than_file_is_not_found() {
        // old_string with more lines than the file used to wrap the loop bound
        // (usize underflow) and panic with an out-of-bounds index.
        let content = "only one line";
        let search = "line one\nline two\nline three";
        let result = smart_replace(content, search, "replacement", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("oldString not found"));
    }

    #[test]
    fn test_replace_all_fuzzy_actually_changes_content() {
        // replace_all with a fuzzy (whitespace) match used to return the
        // content unchanged while reporting success.
        let content = "foo   bar\nbaz";
        let result = smart_replace(content, "foo bar", "qux", true);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "qux\nbaz");
    }
}
