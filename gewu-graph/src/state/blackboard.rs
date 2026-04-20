//! JSON-blackboard state: a generic `State` impl backed by a `serde_json::Value`.
//!
//! Used by `WorkflowCompiler` in `gewu-runtime` so that declarative workflow
//! manifests can pipe untyped JSON between nodes. Typed graphs (e.g. those in
//! `tests/llm_workflow.rs`) are unaffected.
//!
//! Merge semantics: the `Partial` is itself a `Blackboard`, and merging is a
//! recursive object merge. At any given key:
//!
//!   - If both sides are JSON objects, their keys merge (right-hand wins on
//!     overlapping leaves).
//!   - Otherwise the right-hand value replaces the left-hand value.
//!
//! Dotted-path helpers (`get_path`, `set_path`, `from_writes`) let the
//! compiler write node outputs at namespaced locations such as `runs.bazi`.

use crate::state::State;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Blackboard(pub Value);

impl Default for Blackboard {
    fn default() -> Self {
        Blackboard(Value::Object(Map::new()))
    }
}

impl State for Blackboard {
    type Partial = Blackboard;

    fn merge(&self, update: Self::Partial) -> Self {
        let mut out = self.0.clone();
        deep_merge(&mut out, update.0);
        Blackboard(out)
    }
}

impl Blackboard {
    /// Construct a blackboard from an iterator of `(dotted_path, value)` writes.
    ///
    /// Later writes to the same path overwrite earlier ones.
    pub fn from_writes<I>(writes: I) -> Self
    where
        I: IntoIterator<Item = (String, Value)>,
    {
        let mut bb = Self::default();
        for (p, v) in writes {
            bb.set_path(&p, v);
        }
        bb
    }

    /// Wrap a `serde_json::Value` that should be treated as the root object.
    ///
    /// If `value` is not an object, it is placed at the empty root under a
    /// reserved `_` key so that subsequent dotted-path writes still succeed.
    pub fn from_value(value: Value) -> Self {
        if value.is_object() {
            Blackboard(value)
        } else {
            let mut m = Map::new();
            m.insert("_".to_string(), value);
            Blackboard(Value::Object(m))
        }
    }

    pub fn as_value(&self) -> &Value {
        &self.0
    }

    pub fn into_value(self) -> Value {
        self.0
    }

    /// Read a value at a dotted path (`a.b.c`). Returns `None` if any segment
    /// is missing or traverses a non-object.
    pub fn get_path(&self, path: &str) -> Option<&Value> {
        let mut cur = &self.0;
        for seg in path.split('.') {
            if seg.is_empty() {
                continue;
            }
            cur = cur.as_object()?.get(seg)?;
        }
        Some(cur)
    }

    /// Write `value` at a dotted path (`a.b.c`). Intermediate objects are
    /// created as needed. If any intermediate segment exists as a non-object
    /// it is overwritten with an empty object before descending.
    pub fn set_path(&mut self, path: &str, value: Value) {
        let segs: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
        if segs.is_empty() {
            self.0 = value;
            return;
        }
        if !self.0.is_object() {
            self.0 = Value::Object(Map::new());
        }
        let mut cur = &mut self.0;
        for (i, seg) in segs.iter().enumerate() {
            let obj = cur.as_object_mut().expect("must be object");
            if i == segs.len() - 1 {
                obj.insert((*seg).to_string(), value);
                return;
            }
            let entry = obj.entry((*seg).to_string()).or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            cur = entry;
        }
    }

    /// Apply another `Blackboard`'s contents onto `self` in place.
    pub fn merge_in_place(&mut self, update: Blackboard) {
        deep_merge(&mut self.0, update.0);
    }
}

/// Recursive object merge: on overlapping keys, both-objects merge; otherwise
/// `update` replaces `base`.
fn deep_merge(base: &mut Value, update: Value) {
    match (base, update) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                match a.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        a.insert(k, v);
                    }
                }
            }
        }
        (slot, other) => {
            *slot = other;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_is_empty_object() {
        let bb = Blackboard::default();
        assert!(bb.0.is_object());
        assert_eq!(bb.0.as_object().unwrap().len(), 0);
    }

    #[test]
    fn set_and_get_path_simple() {
        let mut bb = Blackboard::default();
        bb.set_path("a.b.c", json!(42));
        assert_eq!(bb.get_path("a.b.c"), Some(&json!(42)));
        assert_eq!(bb.get_path("a.b"), Some(&json!({ "c": 42 })));
        assert_eq!(bb.get_path("missing"), None);
    }

    #[test]
    fn set_path_overwrites_non_object_segment() {
        let mut bb = Blackboard::default();
        bb.set_path("a.b", json!("leaf"));
        bb.set_path("a.b.c", json!(1));
        assert_eq!(bb.get_path("a.b.c"), Some(&json!(1)));
    }

    #[test]
    fn from_writes_namespaces_disjoint_branches() {
        let bb = Blackboard::from_writes(vec![
            ("runs.bazi".to_string(), json!({ "claims": [1] })),
            ("runs.ziwei".to_string(), json!({ "claims": [2] })),
        ]);
        assert_eq!(bb.get_path("runs.bazi.claims"), Some(&json!([1])));
        assert_eq!(bb.get_path("runs.ziwei.claims"), Some(&json!([2])));
    }

    #[test]
    fn merge_is_deep_object_merge() {
        let a = Blackboard(json!({ "x": { "p": 1 }, "y": 2 }));
        let b = Blackboard(json!({ "x": { "q": 3 }, "z": 4 }));
        let m = a.merge(b);
        assert_eq!(m.get_path("x.p"), Some(&json!(1)));
        assert_eq!(m.get_path("x.q"), Some(&json!(3)));
        assert_eq!(m.get_path("y"), Some(&json!(2)));
        assert_eq!(m.get_path("z"), Some(&json!(4)));
    }

    #[test]
    fn merge_scalars_replace() {
        let a = Blackboard(json!({ "x": 1 }));
        let b = Blackboard(json!({ "x": 2 }));
        let m = a.merge(b);
        assert_eq!(m.get_path("x"), Some(&json!(2)));
    }

    #[test]
    fn merge_object_over_scalar_replaces() {
        let a = Blackboard(json!({ "x": 1 }));
        let b = Blackboard(json!({ "x": { "y": 2 } }));
        let m = a.merge(b);
        assert_eq!(m.get_path("x.y"), Some(&json!(2)));
    }

    #[test]
    fn serde_roundtrip() {
        let bb = Blackboard::from_writes(vec![
            ("a.b".to_string(), json!(1)),
            ("c".to_string(), json!("hi")),
        ]);
        let s = serde_json::to_string(&bb).unwrap();
        let back: Blackboard = serde_json::from_str(&s).unwrap();
        assert_eq!(back.0, bb.0);
    }

    #[test]
    fn from_value_with_non_object_wraps_under_underscore() {
        let bb = Blackboard::from_value(json!(7));
        assert_eq!(bb.get_path("_"), Some(&json!(7)));
    }
}
