//! YAML schema for workflow manifests.
//!
//! A `WorkflowManifest` is the declarative counterpart of the imperative
//! `StateGraph` builder. A manifest is compiled into a
//! `CompiledGraph<Blackboard>` by `crate::workflow::WorkflowCompiler`.
//!
//! See `docs/2026-04-20-self-assembling-workflows.md` for the design.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Top-level workflow manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowManifest {
    pub id: String,
    #[serde(default)]
    pub description: String,
    pub entry: String,
    #[serde(default)]
    pub finish: Vec<String>,
    pub nodes: BTreeMap<String, NodeSpec>,
    #[serde(default)]
    pub edges: Vec<EdgeSpec>,
    #[serde(default)]
    pub metadata: Value,
}

/// A single node in a workflow.
///
/// Four kinds, discriminated by a `kind:` tag:
///
/// - `specialist`: invoke a specialist via the runtime, optionally pinning a
///   specific tool the specialist must call via its prompt.
/// - `tool`: invoke a capability directly through the registry (no LLM).
/// - `parallel`: spawn a set of branch nodes concurrently; each branch's
///   output is written to the blackboard namespaced under the branch id.
/// - `workflow`: nest another compiled workflow as a subgraph.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeSpec {
    Specialist {
        specialist: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        write: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    Tool {
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        write: Option<String>,
    },
    Parallel {
        branches: BTreeMap<String, NodeSpec>,
    },
    Workflow {
        workflow: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        write: Option<String>,
    },
}

/// A workflow edge.
///
/// Two shapes: a plain `[from, to]` two-tuple or a conditional object with a
/// `when` predicate (see `crate::workflow::predicate`) and optional
/// `max_loops` cap for self-cycling edges.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum EdgeSpec {
    Plain([String; 2]),
    Conditional(ConditionalEdgeSpec),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConditionalEdgeSpec {
    pub from: String,
    pub to: String,
    pub when: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_loops: Option<u32>,
}

impl EdgeSpec {
    pub fn from(&self) -> &str {
        match self {
            EdgeSpec::Plain([a, _]) => a,
            EdgeSpec::Conditional(c) => &c.from,
        }
    }

    pub fn to(&self) -> &str {
        match self {
            EdgeSpec::Plain([_, b]) => b,
            EdgeSpec::Conditional(c) => &c.to,
        }
    }

    pub fn is_conditional(&self) -> bool {
        matches!(self, EdgeSpec::Conditional(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_workflow() {
        let yaml = r#"
id: minimal
entry: a
finish: [b]
nodes:
  a:
    kind: specialist
    specialist: speaker
    write: greeting
  b:
    kind: tool
    tool: report
    input: $.greeting
edges:
  - [a, b]
"#;
        let m: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(m.id, "minimal");
        assert_eq!(m.entry, "a");
        assert_eq!(m.finish, vec!["b".to_string()]);
        assert_eq!(m.nodes.len(), 2);
        match &m.nodes["a"] {
            NodeSpec::Specialist { specialist, .. } => assert_eq!(specialist, "speaker"),
            _ => panic!("expected specialist node"),
        }
        assert_eq!(m.edges.len(), 1);
        assert_eq!(m.edges[0].from(), "a");
        assert_eq!(m.edges[0].to(), "b");
    }

    #[test]
    fn parse_conditional_edge_with_max_loops() {
        let yaml = r#"
id: loopy
entry: a
finish: [c]
nodes:
  a: { kind: tool, tool: t }
  b: { kind: tool, tool: t }
  c: { kind: tool, tool: t }
edges:
  - [a, b]
  - { from: b, to: a, when: "$.x == true", max_loops: 2 }
  - { from: b, to: c, when: "$.x == false" }
"#;
        let m: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(m.edges.len(), 3);
        assert!(m.edges[1].is_conditional());
        match &m.edges[1] {
            EdgeSpec::Conditional(c) => {
                assert_eq!(c.from, "b");
                assert_eq!(c.to, "a");
                assert_eq!(c.when, "$.x == true");
                assert_eq!(c.max_loops, Some(2));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_parallel_and_workflow_nodes() {
        let yaml = r#"
id: nested
entry: dispatch
finish: [done]
nodes:
  dispatch:
    kind: parallel
    branches:
      left:  { kind: specialist, specialist: L, write: runs.left }
      right: { kind: specialist, specialist: R, write: runs.right }
  sub:
    kind: workflow
    workflow: other_flow
    input: $.runs
    write: sub_out
  done:
    kind: tool
    tool: noop
edges:
  - [dispatch, sub]
  - [sub, done]
"#;
        let m: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
        match &m.nodes["dispatch"] {
            NodeSpec::Parallel { branches } => {
                assert_eq!(branches.len(), 2);
                assert!(branches.contains_key("left"));
                assert!(branches.contains_key("right"));
            }
            _ => panic!("expected parallel"),
        }
        match &m.nodes["sub"] {
            NodeSpec::Workflow { workflow, .. } => assert_eq!(workflow, "other_flow"),
            _ => panic!("expected workflow"),
        }
    }

    #[test]
    fn roundtrip_via_yaml() {
        let manifest = WorkflowManifest {
            id: "x".into(),
            description: "d".into(),
            entry: "a".into(),
            finish: vec!["a".into()],
            nodes: {
                let mut m = BTreeMap::new();
                m.insert(
                    "a".into(),
                    NodeSpec::Tool {
                        tool: "noop".into(),
                        input: None,
                        write: Some("r".into()),
                    },
                );
                m
            },
            edges: vec![],
            metadata: Value::Null,
        };
        let s = serde_yaml::to_string(&manifest).unwrap();
        let back: WorkflowManifest = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, manifest);
    }
}
