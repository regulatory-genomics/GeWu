//! Prompt template rendering for workflow specs.
//!
//! The vocabulary is fixed and small: `{{item}}`, `{{index}}`, `{{output}}`
//! (inside a review prompt), and per-node references `{{nodes.<id>.outputs}}`,
//! `{{nodes.<id>.failed}}`, `{{nodes.<id>.status}}`, and
//! `{{nodes.<id>.failed_count}}` (routing templates may use all four). A
//! reference to an unknown variable is an error, never an empty string —
//! the model should learn about the mistake from the validation error, not
//! discover it in a mangled prompt.

use std::collections::BTreeMap;

/// Values available to a prompt template.
#[derive(Debug, Clone, Default)]
pub struct TemplateVars {
    /// The current `for_each` item, rendered (strings as-is, other JSON
    /// compact).
    pub item: Option<String>,
    /// Zero-based position of the current item within its node.
    pub index: Option<usize>,
    /// The current task's output (review prompts only).
    pub output: Option<String>,
    /// Outcomes of completed nodes, keyed by node id.
    pub nodes: BTreeMap<String, NodeTemplateData>,
}

/// A completed node's results, as addressable from templates.
#[derive(Debug, Clone, Default)]
pub struct NodeTemplateData {
    /// Successful task outputs, in item order.
    pub outputs: Vec<String>,
    /// Failed task records (JSON objects with `index`, `item`, `error`).
    pub failed: Vec<serde_json::Value>,
    /// `"ok"` or `"failed"` — used by routing templates.
    pub status: String,
}

/// Checks that every template reference in `prompt` is part of the fixed
/// vocabulary and resolvable in this context: `{{item}}`/`{{index}}` need a
/// `for_each` node, `{{output}}` a review prompt, and `{{nodes.<id>...}}`
/// must point at one of `known_nodes`.
pub fn validate_prompt(
    prompt: &str,
    has_items: bool,
    has_output: bool,
    known_nodes: &[&str],
) -> Result<(), String> {
    for expression in expressions(prompt)? {
        match expression.as_str() {
            "item" | "index" if !has_items => {
                return Err(format!(
                    "{{{{{expression}}}}} is only available in for_each nodes"
                ));
            }
            "output" if !has_output => {
                return Err("{{output}} is only available in review prompts".to_string());
            }
            name => {
                let Some((id, field)) = name.strip_prefix("nodes.").and_then(split_field) else {
                    if name == "item" || name == "index" || name == "output" {
                        continue;
                    }
                    return Err(format!("unknown template variable '{{{{{name}}}}}'"));
                };
                if !matches!(field, "outputs" | "failed" | "status" | "failed_count") {
                    return Err(format!(
                        "unknown template variable '{{{{{name}}}}}' (use .outputs, .failed, .status, or .failed_count)"
                    ));
                }
                if !known_nodes.contains(&id) {
                    return Err(format!(
                        "unknown node '{id}' in template variable '{{{{{name}}}}}'"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Renders `prompt` by substituting the template variables. Any reference
/// that has no value in `vars` is an error.
pub fn render(prompt: &str, vars: &TemplateVars) -> Result<String, String> {
    let mut rendered = String::with_capacity(prompt.len());
    let mut rest = prompt;
    for expression in expressions(prompt)? {
        let (before, after) = rest.split_once("{{").expect("expression implies '{{'");
        rendered.push_str(before);
        let (_, remainder) = after.split_once("}}").expect("expression implies '}}'");
        rest = remainder;
        rendered.push_str(&resolve(&expression, vars)?);
    }
    rendered.push_str(rest);
    Ok(rendered)
}

fn resolve(expression: &str, vars: &TemplateVars) -> Result<String, String> {
    match expression {
        "item" => vars
            .item
            .clone()
            .ok_or_else(|| "{{item}} has no value in this context".to_string()),
        "index" => vars
            .index
            .map(|index| index.to_string())
            .ok_or_else(|| "{{index}} has no value in this context".to_string()),
        "output" => vars
            .output
            .clone()
            .ok_or_else(|| "{{output}} has no value in this context".to_string()),
        name => {
            let Some((id, field)) = name.strip_prefix("nodes.").and_then(split_field) else {
                return Err(format!("unknown template variable '{{{{{name}}}}}'"));
            };
            let Some(node) = vars.nodes.get(id) else {
                return Err(format!("node '{id}' has not run yet (no outcome)"));
            };
            match field {
                "outputs" => serde_json::to_string(&node.outputs)
                    .map_err(|e| format!("failed to render {name}: {e}")),
                "failed" => serde_json::to_string(&node.failed)
                    .map_err(|e| format!("failed to render {name}: {e}")),
                "status" => Ok(node.status.clone()),
                "failed_count" => Ok(node.failed.len().to_string()),
                _ => Err(format!("unknown template variable '{{{{{name}}}}}'")),
            }
        }
    }
}

/// Extracts the trimmed expressions between `{{` and `}}` delimiters, in
/// order. An unterminated `{{` is an error.
fn expressions(prompt: &str) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    let mut rest = prompt;
    while let Some((_, after)) = rest.split_once("{{") {
        let Some((expression, remainder)) = after.split_once("}}") else {
            return Err("unterminated template: missing '}}'".to_string());
        };
        found.push(expression.trim().to_string());
        rest = remainder;
    }
    Ok(found)
}

fn split_field(rest: &str) -> Option<(&str, &str)> {
    let (id, field) = rest.rsplit_once('.')?;
    Some((id, field))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> TemplateVars {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            "read".to_string(),
            NodeTemplateData {
                outputs: vec!["found X".into(), "found Y".into()],
                failed: vec![serde_json::json!({"index": 2, "item": "c.md", "error": "boom"})],
                status: "failed".into(),
            },
        );
        TemplateVars {
            item: Some("a.md".into()),
            index: Some(0),
            output: Some("the review".into()),
            nodes,
        }
    }

    #[test]
    fn renders_the_full_vocabulary() {
        let vars = vars();
        assert_eq!(
            render("Read {{item}} at #{{index}}", &vars).unwrap(),
            "Read a.md at #0"
        );
        assert_eq!(render("{{output}}", &vars).unwrap(), "the review");
        let combined = render("{{nodes.read.outputs}}", &vars).unwrap();
        assert!(combined.contains("found X") && combined.contains("found Y"));
        let failed = render("{{nodes.read.failed}}", &vars).unwrap();
        assert!(failed.contains("c.md") && failed.contains("boom"));
        assert_eq!(render("{{nodes.read.status}}", &vars).unwrap(), "failed");
        assert_eq!(render("{{nodes.read.failed_count}}", &vars).unwrap(), "1");
    }

    #[test]
    fn without_a_match_the_prompt_is_kept_verbatim() {
        assert_eq!(
            render("no templates here", &vars()).unwrap(),
            "no templates here"
        );
    }

    #[test]
    fn missing_values_are_errors_not_empty_strings() {
        let empty = TemplateVars::default();
        for prompt in [
            "{{item}}",
            "{{index}}",
            "{{output}}",
            "{{nodes.read.outputs}}",
            "{{nodes.read.status}}",
        ] {
            let err = render(prompt, &empty).unwrap_err();
            assert!(!err.is_empty(), "{prompt}");
        }
    }

    #[test]
    fn unknown_variables_are_errors() {
        for prompt in [
            "{{bogus}}",
            "{{nodes.read.bogus}}",
            "{{nodes.zzz.outputs}}",
            "{{phases.read.outputs}}",
            "{{}}",
        ] {
            let err = render(prompt, &vars()).unwrap_err();
            assert!(
                err.contains("unknown") || err.contains("has not run"),
                "{prompt}: {err}"
            );
        }
    }

    #[test]
    fn unterminated_templates_are_errors() {
        assert!(render("oops {{item", &vars()).is_err());
    }

    #[test]
    fn validation_matches_context() {
        assert!(validate_prompt("{{item}}", true, false, &[]).is_ok());
        assert!(validate_prompt("{{item}}", false, false, &[]).is_err());
        assert!(validate_prompt("{{output}}", false, true, &[]).is_ok());
        assert!(validate_prompt("{{output}}", false, false, &[]).is_err());
        assert!(validate_prompt("{{nodes.a.outputs}}", false, false, &["a"]).is_ok());
        assert!(validate_prompt("{{nodes.a.outputs}}", false, false, &["b"]).is_err());
        assert!(validate_prompt("{{nodes.a.status}}", false, false, &["a"]).is_ok());
        assert!(validate_prompt("{{nodes.a.failed_count}}", false, false, &["a"]).is_ok());
        assert!(validate_prompt("{{nodes.a.outputs}}", false, false, &[]).is_err());
        assert!(validate_prompt("{{bogus}}", true, true, &["a"]).is_err());
    }
}
