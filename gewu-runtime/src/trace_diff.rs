//! Structural diff between two `Blackboard`s, ignoring opt-out paths.
//!
//! The workflow validation plan (§6 of the design doc) relies on comparing
//! the final blackboard of a Rust-path run against a workflow-compiled run.
//! Because the two paths may legitimately differ in ephemeral fields (error
//! scratchpads, loop counters, timestamps inside nested values), the
//! comparison runs with a configurable ignore-list.

use gewu_graph::Blackboard;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffKind {
    Added,   // present in rhs, absent in lhs
    Removed, // present in lhs, absent in rhs
    Changed, // both present, values differ
}

#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub path: String,
    pub kind: DiffKind,
    pub lhs: Option<Value>,
    pub rhs: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct DiffOptions {
    /// Paths to fully ignore (exact match or prefix match — `foo.bar` ignores
    /// `foo.bar` and `foo.bar.*`). Namespacing `_` (e.g. `_errors`, `_loops`,
    /// `_meta`) is suggested.
    pub ignore_prefixes: Vec<String>,
}

impl DiffOptions {
    pub fn ignoring<I, S>(prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            ignore_prefixes: prefixes.into_iter().map(Into::into).collect(),
        }
    }

    pub fn is_ignored(&self, path: &str) -> bool {
        self.ignore_prefixes
            .iter()
            .any(|p| path == p || path.starts_with(&format!("{}.", p)))
    }
}

/// Produce a list of differences between two blackboards. Empty result means
/// they are structurally equal (modulo `opts.ignore_prefixes`).
pub fn diff_blackboards(lhs: &Blackboard, rhs: &Blackboard, opts: &DiffOptions) -> Vec<DiffEntry> {
    let mut out = Vec::new();
    walk_diff(lhs.as_value(), rhs.as_value(), "", opts, &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn walk_diff(lhs: &Value, rhs: &Value, path: &str, opts: &DiffOptions, out: &mut Vec<DiffEntry>) {
    if opts.is_ignored(path) {
        return;
    }
    match (lhs, rhs) {
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: std::collections::BTreeSet<&String> = a.keys().collect();
            keys.extend(b.keys());
            for k in keys {
                let sub_path = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", path, k)
                };
                match (a.get(k), b.get(k)) {
                    (Some(av), Some(bv)) => walk_diff(av, bv, &sub_path, opts, out),
                    (Some(av), None) => {
                        if !opts.is_ignored(&sub_path) {
                            out.push(DiffEntry {
                                path: sub_path,
                                kind: DiffKind::Removed,
                                lhs: Some(av.clone()),
                                rhs: None,
                            });
                        }
                    }
                    (None, Some(bv)) => {
                        if !opts.is_ignored(&sub_path) {
                            out.push(DiffEntry {
                                path: sub_path,
                                kind: DiffKind::Added,
                                lhs: None,
                                rhs: Some(bv.clone()),
                            });
                        }
                    }
                    (None, None) => {}
                }
            }
        }
        (a, b) if a == b => {}
        (a, b) => out.push(DiffEntry {
            path: path.to_string(),
            kind: DiffKind::Changed,
            lhs: Some(a.clone()),
            rhs: Some(b.clone()),
        }),
    }
}

/// Render a compact diff report (one line per entry) for test failure output.
pub fn format_diff(diffs: &[DiffEntry]) -> String {
    let mut s = String::new();
    for d in diffs {
        match d.kind {
            DiffKind::Added => {
                s.push_str(&format!("+ {}: {}\n", d.path, d.rhs.as_ref().unwrap()));
            }
            DiffKind::Removed => {
                s.push_str(&format!("- {}: {}\n", d.path, d.lhs.as_ref().unwrap()));
            }
            DiffKind::Changed => {
                s.push_str(&format!(
                    "~ {}: {} → {}\n",
                    d.path,
                    d.lhs.as_ref().unwrap(),
                    d.rhs.as_ref().unwrap()
                ));
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identical_blackboards_have_no_diff() {
        let a = Blackboard(json!({ "x": 1, "y": { "z": "ok" } }));
        let b = Blackboard(json!({ "x": 1, "y": { "z": "ok" } }));
        assert!(diff_blackboards(&a, &b, &DiffOptions::default()).is_empty());
    }

    #[test]
    fn added_and_removed_keys_are_reported() {
        let a = Blackboard(json!({ "x": 1 }));
        let b = Blackboard(json!({ "y": 2 }));
        let d = diff_blackboards(&a, &b, &DiffOptions::default());
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].path, "x");
        assert_eq!(d[0].kind, DiffKind::Removed);
        assert_eq!(d[1].path, "y");
        assert_eq!(d[1].kind, DiffKind::Added);
    }

    #[test]
    fn changed_leaf_is_reported_with_both_values() {
        let a = Blackboard(json!({ "x": 1 }));
        let b = Blackboard(json!({ "x": 2 }));
        let d = diff_blackboards(&a, &b, &DiffOptions::default());
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, DiffKind::Changed);
        assert_eq!(d[0].lhs, Some(json!(1)));
        assert_eq!(d[0].rhs, Some(json!(2)));
    }

    #[test]
    fn ignore_prefix_suppresses_subtree() {
        let a = Blackboard(json!({ "x": 1, "_errors": { "a": "boom" } }));
        let b = Blackboard(json!({ "x": 1 }));
        let d = diff_blackboards(&a, &b, &DiffOptions::ignoring(vec!["_errors".to_string()]));
        assert!(d.is_empty(), "got: {:?}", d);
    }

    #[test]
    fn format_diff_is_readable() {
        let a = Blackboard(json!({ "x": 1 }));
        let b = Blackboard(json!({ "x": 2, "y": 3 }));
        let d = diff_blackboards(&a, &b, &DiffOptions::default());
        let s = format_diff(&d);
        assert!(s.contains("~ x:"));
        assert!(s.contains("+ y:"));
    }
}
