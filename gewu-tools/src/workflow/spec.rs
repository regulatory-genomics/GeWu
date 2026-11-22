//! Workflow specification: serde/schemars types and validation.
//!
//! A [`WorkflowSpec`] is the JSON document the LLM writes and starts with
//! the `workflow_start` tool. It describes a `gewu-graph` state graph:
//! named nodes, optional edges, optional conditional routing. Everything
//! checkable before any agent spawns is checked here, so spec errors go
//! back to the model instead of appearing mid-run.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Starter document shared by the web and terminal designers.
pub const EXAMPLE: &str = include_str!("../../examples/busco-summary.json");

/// Extract a workflow from a plain JSON or fenced assistant response.
pub fn from_assistant_reply(reply: &str) -> anyhow::Result<serde_json::Value> {
    let documents =
        std::iter::once(reply.trim()).chain(reply.split("```").skip(1).step_by(2).map(|block| {
            let block = block.trim();
            block.strip_prefix("json").unwrap_or(block).trim()
        }));
    for document in documents {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(document) {
            let spec = value.get("spec").unwrap_or(&value);
            if serde_json::from_value::<WorkflowSpec>(spec.clone()).is_ok() {
                return Ok(spec.clone());
            }
        }
    }
    anyhow::bail!("The last reply needs a workflow specification in a JSON code block")
}

/// Run time limit when the spec does not set one.
pub const DEFAULT_TIMEOUT_SECS: u64 = 1800;
/// Hard cap on `timeout_secs`.
pub const MAX_TIMEOUT_SECS: u64 = 7200;
/// Tasks of one node running at once when the spec does not set a bound.
pub const DEFAULT_CONCURRENCY: usize = 4;
/// Hard cap on `max_concurrency`.
pub const MAX_CONCURRENCY: usize = 16;
/// Agent tasks per run, counting reviewer calls and redos.
pub const MAX_TASKS_PER_RUN: usize = 50;
/// Graph supersteps per run when the spec does not set `max_steps`.
pub const DEFAULT_MAX_STEPS: usize = 1000;
/// Hard cap on `max_steps`.
pub const MAX_STEPS: usize = 10_000;

/// Reserved routing target: the graph's exit.
pub const ROUTE_END: &str = "END";
/// Reserved default key of a conditional edge's `map`.
pub const ROUTE_DEFAULT: &str = "*";

/// A workflow specification: a graph of agent nodes compiled onto a
/// `gewu-graph` `StateGraph`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSpec {
    /// Short human-readable name for the run (shown in status output).
    #[serde(default)]
    pub name: Option<String>,
    /// Whole-run time limit in seconds (default 1800, capped at 7200).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Tasks of one node running at once (default 4, capped at 16).
    #[serde(default)]
    pub max_concurrency: Option<usize>,
    /// Inline agent definitions, keyed by name. Node and reviewer agents
    /// may reference these or the predefined profiles.
    #[serde(default)]
    pub agents: BTreeMap<String, InlineAgent>,
    /// The graph's nodes, in declaration order. Each node runs its agent
    /// tasks (one, or one per `for_each` item) to completion.
    pub nodes: Vec<NodeSpec>,
    /// Directed edges as `[from, to]` pairs. Omitted edges chain the
    /// nodes in declaration order (`nodes[0] -> nodes[1] -> ...`). A node
    /// with several incoming edges waits for all of them (join barrier).
    /// Targets may also be `"END"`.
    #[serde(default)]
    pub edges: Option<Vec<(String, String)>>,
    /// Conditional routing: keyed by source node id. A node with a
    /// conditional edge routes only via its `map` — its normal outgoing
    /// edges are replaced, not added to.
    #[serde(default)]
    pub conditional_edges: BTreeMap<String, RouteSpec>,
    /// First node to run (default: the first declared node).
    #[serde(default)]
    pub entry: Option<String>,
    /// Maximum graph supersteps (default 1000, capped at 10000) — bounds
    /// loops and unreachable-node wind-down.
    #[serde(default)]
    pub max_steps: Option<usize>,
}

/// An inline agent: a base profile, optionally with extra instructions and
/// a narrowed tool list. `tools` can only remove tools from the base
/// profile, never add them.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InlineAgent {
    /// Base profile to derive from (e.g. `build`, `explore`, `general`).
    pub base: String,
    /// Extra instructions appended to the base profile's system prompt.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Tool names to keep from the base profile; unknown names are
    /// rejected, so an inline agent cannot gain privileges.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
}

/// One graph node: an agent run once, or once per item via `for_each`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeSpec {
    /// Unique node id, referenced by `for_each: {"from": ...}`, by the
    /// `{{nodes.<id>...}}` templates, by `edges`, and by routing.
    pub id: String,
    /// Agent name: a predefined profile or a key of `agents`.
    pub agent: String,
    /// Run once per item: a literal list, or the results of another node
    /// (`{"from": "<node id>"}`).
    #[serde(default)]
    pub for_each: Option<ForEach>,
    /// Prompt for each task. Supports the fixed template vocabulary:
    /// `{{item}}`, `{{index}}`, and `{{nodes.<id>.outputs}}` /
    /// `{{nodes.<id>.failed}}` / `{{nodes.<id>.status}}` /
    /// `{{nodes.<id>.failed_count}}` for nodes that have already run.
    pub prompt: String,
    /// Optional reviewer pass with redo.
    #[serde(default)]
    pub review: Option<ReviewSpec>,
}

/// The item list of a `for_each` node.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ForEach {
    /// One task per result of another node.
    From {
        /// Id of a node whose successful outputs are the items. If that
        /// node has not run yet, this node runs with zero tasks.
        from: String,
    },
    /// One task per literal item (strings recommended).
    Items(Vec<serde_json::Value>),
}

/// A reviewer pass over each task's output. If the reviewer's response does
/// not start with `pass_if`, the task re-runs with the feedback, up to
/// `max_redos` times.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewSpec {
    /// Reviewer agent name: a predefined profile or a key of `agents`.
    pub agent: String,
    /// Review prompt. Supports `{{item}}`, `{{index}}`, and `{{output}}`
    /// (the task's output).
    pub prompt: String,
    /// The reviewer's output must start with this token to pass
    /// (default `"PASS"`).
    #[serde(default = "default_pass_if")]
    pub pass_if: String,
    /// Re-run budget after a rejection (default 1).
    #[serde(default = "default_max_redos")]
    pub max_redos: u32,
}

/// Conditional routing for one source node: a rendered `path` selects a
/// target from `map`. The special key `"*"` matches any unlisted value;
/// with no match (and no `"*"`), execution ends.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteSpec {
    /// Routing template, rendered after the node completes. Either
    /// `{{nodes.<id>.status}}` (renders `ok` or `failed`) or
    /// `{{nodes.<id>.failed_count}}` (renders the failure count).
    pub path: String,
    /// Rendered key to target node id (or `"END"`). `"*"` is the fallback
    /// for any other key.
    pub map: BTreeMap<String, String>,
}

fn default_pass_if() -> String {
    "PASS".to_string()
}

fn default_max_redos() -> u32 {
    1
}

impl WorkflowSpec {
    /// Resolved run time limit.
    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS)
    }

    /// Resolved per-node concurrency bound.
    pub fn concurrency(&self) -> usize {
        self.max_concurrency
            .unwrap_or(DEFAULT_CONCURRENCY)
            .clamp(1, MAX_CONCURRENCY)
    }

    /// Resolved superstep bound.
    pub fn steps(&self) -> usize {
        self.max_steps
            .unwrap_or(DEFAULT_MAX_STEPS)
            .clamp(1, MAX_STEPS)
    }

    /// The id of the first node to run.
    pub fn entry_node(&self) -> &str {
        self.entry
            .as_deref()
            .unwrap_or_else(|| self.nodes[0].id.as_str())
    }

    /// Resolved edges: explicit pairs, or the declaration-order chain when
    /// `edges` is absent. Used by validation and compilation.
    pub fn resolved_edges(&self) -> Vec<(String, String)> {
        match &self.edges {
            Some(pairs) => pairs.clone(),
            None => self
                .nodes
                .windows(2)
                .map(|pair| (pair[0].id.clone(), pair[1].id.clone()))
                .collect(),
        }
    }

    /// Structural validation, checked before anything spawns:
    ///
    /// - at least one node, unique node ids
    /// - node and reviewer agents are predefined profiles (passed in as
    ///   `builtin_agents`) or keys of the spec's `agents` map
    /// - edges connect declared nodes (targets may be `"END"`)
    /// - `conditional_edges` paths use the routing vocabulary and their
    ///   map targets are declared nodes or `"END"`
    /// - `entry` names a declared node
    /// - template references resolve: `{{item}}`/`{{index}}` only in
    ///   `for_each` prompts, `{{output}}` only in review prompts, and
    ///   `{{nodes.<id>...}}` pointing at declared nodes
    pub fn validate(&self, builtin_agents: &[&str]) -> Result<(), String> {
        if self.nodes.is_empty() {
            return Err("a workflow needs at least one node".to_string());
        }
        let ids: Vec<&str> = self.nodes.iter().map(|node| node.id.as_str()).collect();
        let mut seen: Vec<&str> = Vec::new();
        for (position, node) in self.nodes.iter().enumerate() {
            if node.id.is_empty() {
                return Err(format!("node {position}: id must not be empty"));
            }
            if seen.contains(&node.id.as_str()) {
                return Err(format!("duplicate node id '{}'", node.id));
            }
            self.validate_agent(&node.agent, builtin_agents)
                .map_err(|e| format!("node '{}': {e}", node.id))?;
            if let Some(ForEach::From { from }) = &node.for_each
                && !ids.contains(&from.as_str())
            {
                return Err(format!(
                    "node '{}': for_each 'from' must reference a declared node, got '{}'",
                    node.id, from
                ));
            }
            crate::workflow::template::validate_prompt(
                &node.prompt,
                node.for_each.is_some(),
                false,
                &ids,
            )
            .map_err(|e| format!("node '{}': {e}", node.id))?;
            if let Some(review) = &node.review {
                self.validate_agent(&review.agent, builtin_agents)
                    .map_err(|e| format!("node '{}' review: {e}", node.id))?;
                crate::workflow::template::validate_prompt(
                    &review.prompt,
                    node.for_each.is_some(),
                    true,
                    &ids,
                )
                .map_err(|e| format!("node '{}' review: {e}", node.id))?;
                if review.pass_if.is_empty() {
                    return Err(format!(
                        "node '{}': review pass_if must not be empty",
                        node.id
                    ));
                }
            }
            seen.push(&node.id);
        }

        for (from, to) in self.resolved_edges() {
            if !ids.contains(&from.as_str()) {
                return Err(format!("edge '{from} -> {to}': unknown node '{from}'"));
            }
            if to != ROUTE_END && !ids.contains(&to.as_str()) {
                return Err(format!("edge '{from} -> {to}': unknown node '{to}'"));
            }
            if from == ROUTE_END {
                return Err(format!("edge '{from} -> {to}': 'END' cannot be a source"));
            }
        }

        for (source, route) in &self.conditional_edges {
            if !ids.contains(&source.as_str()) {
                return Err(format!("conditional_edges: unknown node '{source}'"));
            }
            validate_route_path(&route.path, &ids)
                .map_err(|e| format!("conditional_edges '{source}': {e}"))?;
            if route.map.is_empty() {
                return Err(format!(
                    "conditional_edges '{source}': map must not be empty"
                ));
            }
            for (key, target) in &route.map {
                if key != ROUTE_DEFAULT && target != ROUTE_END && !ids.contains(&target.as_str()) {
                    return Err(format!(
                        "conditional_edges '{source}': map key '{key}' targets unknown node '{target}'"
                    ));
                }
            }
        }

        if !ids.contains(&self.entry_node()) {
            return Err(format!("entry: unknown node '{}'", self.entry_node()));
        }
        Ok(())
    }

    fn validate_agent(&self, name: &str, builtin_agents: &[&str]) -> Result<(), String> {
        if builtin_agents.contains(&name) || self.agents.contains_key(name) {
            Ok(())
        } else {
            Err(format!(
                "unknown agent '{name}' (use a predefined profile or define it in 'agents')"
            ))
        }
    }
}

/// Checks that `path` is exactly one routing template expression:
/// `{{nodes.<id>.status}}` or `{{nodes.<id>.failed_count}}` for a declared
/// node.
fn validate_route_path(path: &str, node_ids: &[&str]) -> Result<(), String> {
    let expression = path
        .trim()
        .strip_prefix("{{")
        .and_then(|rest| rest.strip_suffix("}}"))
        .map(str::trim)
        .ok_or_else(|| {
            "path must be a single template like \"{{nodes.<id>.status}}\"".to_string()
        })?;
    if expression.contains("{{") {
        return Err("path must be a single template expression".to_string());
    }
    let Some((id, field)) = expression
        .strip_prefix("nodes.")
        .and_then(|rest| rest.rsplit_once('.'))
    else {
        return Err(format!(
            "path must be \"{{{{nodes.<id>.status}}}}\" or \"{{{{nodes.<id>.failed_count}}}}\", got '{expression}'"
        ));
    };
    if field != "status" && field != "failed_count" {
        return Err(format!(
            "path field must be .status or .failed_count, got '.{field}'"
        ));
    }
    if !node_ids.contains(&id) {
        return Err(format!("path references unknown node '{id}'"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BUILTINS: &[&str] = &["build", "explore", "general"];

    fn spec(value: serde_json::Value) -> WorkflowSpec {
        serde_json::from_value(value).expect("spec should parse")
    }

    fn minimal_nodes() -> serde_json::Value {
        json!([{"id": "one", "agent": "general", "prompt": "hello"}])
    }

    #[test]
    fn parses_the_documented_example() {
        let spec = spec(json!({
            "name": "contract-review",
            "timeout_secs": 3600,
            "max_concurrency": 8,
            "agents": {
                "reader": {
                    "base": "explore",
                    "instructions": "You review contracts...",
                    "tools": ["read_file", "grep"]
                }
            },
            "nodes": [
                {
                    "id": "read",
                    "agent": "reader",
                    "for_each": ["a.md", "b.md"],
                    "prompt": "Review {{item}}. List risky clauses.",
                    "review": {
                        "agent": "general",
                        "prompt": "Check this review of {{item}}:\n{{output}}\nStart with PASS or FAIL.",
                        "pass_if": "PASS",
                        "max_redos": 1
                    }
                },
                {
                    "id": "combine",
                    "agent": "general",
                    "prompt": "Combine these findings:\n{{nodes.read.outputs}}"
                }
            ]
        }));
        assert_eq!(spec.nodes.len(), 2);
        assert_eq!(spec.agents["reader"].base, "explore");
        assert!(spec.validate(BUILTINS).is_ok());
        assert_eq!(spec.timeout_secs(), 3600);
        assert_eq!(spec.concurrency(), 8);
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = serde_json::from_value::<WorkflowSpec>(json!({
            "nodes": minimal_nodes(),
            "bogus": true
        }))
        .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn applies_defaults_and_caps() {
        let mut spec = spec(json!({ "nodes": minimal_nodes() }));
        assert_eq!(spec.timeout_secs(), DEFAULT_TIMEOUT_SECS);
        assert_eq!(spec.concurrency(), DEFAULT_CONCURRENCY);
        assert_eq!(spec.steps(), DEFAULT_MAX_STEPS);
        assert_eq!(spec.entry_node(), "one");
        spec.timeout_secs = Some(100_000);
        spec.max_concurrency = Some(0);
        spec.max_steps = Some(0);
        assert_eq!(spec.timeout_secs(), MAX_TIMEOUT_SECS);
        assert_eq!(spec.concurrency(), 1);
        assert_eq!(spec.steps(), 1);
        spec.max_steps = Some(500_000);
        assert_eq!(spec.steps(), MAX_STEPS);
    }

    #[test]
    fn absent_edges_chain_in_declaration_order() {
        let spec = spec(json!({
            "nodes": [
                {"id": "a", "agent": "general", "prompt": "p"},
                {"id": "b", "agent": "general", "prompt": "p"},
                {"id": "c", "agent": "general", "prompt": "p"}
            ]
        }));
        assert_eq!(
            spec.resolved_edges(),
            vec![
                ("a".to_string(), "b".to_string()),
                ("b".to_string(), "c".to_string())
            ]
        );
    }

    #[test]
    fn validation_rejects_structural_errors() {
        let cases: Vec<(serde_json::Value, &str)> = vec![
            (json!({ "nodes": [] }), "at least one node"),
            (
                json!({ "nodes": [minimal_nodes()[0], minimal_nodes()[0]] }),
                "duplicate node id",
            ),
            (
                json!({ "nodes": [{"id": "a", "agent": "missing", "prompt": "p"}] }),
                "unknown agent 'missing'",
            ),
            (
                json!({ "nodes": [{"id": "a", "agent": "general",
                    "for_each": {"from": "zzz"}, "prompt": "p"}] }),
                "must reference a declared node",
            ),
            (
                json!({ "nodes": [{"id": "a", "agent": "general", "prompt": "{{bogus}}"}] }),
                "unknown template variable",
            ),
            (
                json!({ "nodes": [{"id": "a", "agent": "general", "prompt": "{{item}}"}] }),
                "{{item}}",
            ),
            (
                json!({ "nodes": [{"id": "a", "agent": "general", "prompt": "p",
                    "review": {"agent": "general", "prompt": "check {{nodes.zzz.outputs}}"}}] }),
                "unknown node",
            ),
            (
                json!({ "nodes": [{"id": "a", "agent": "general", "prompt": "p"},
                    {"id": "b", "agent": "general", "prompt": "p"}],
                    "edges": [["a", "zzz"]] }),
                "unknown node 'zzz'",
            ),
            (
                json!({ "nodes": [{"id": "a", "agent": "general", "prompt": "p"},
                    {"id": "b", "agent": "general", "prompt": "p"}],
                    "edges": [["zzz", "b"]] }),
                "unknown node 'zzz'",
            ),
            (
                json!({ "nodes": [{"id": "a", "agent": "general", "prompt": "p"}],
                    "conditional_edges": {"zzz": {
                        "path": "{{nodes.a.status}}", "map": {"ok": "END"}}} }),
                "unknown node 'zzz'",
            ),
            (
                json!({ "nodes": [{"id": "a", "agent": "general", "prompt": "p"}],
                    "conditional_edges": {"a": {"path": "nodes.a.status",
                        "map": {"ok": "END"}}} }),
                "single template",
            ),
            (
                json!({ "nodes": [{"id": "a", "agent": "general", "prompt": "p"}],
                    "conditional_edges": {"a": {"path": "{{nodes.zzz.status}}",
                        "map": {"ok": "END"}}} }),
                "unknown node 'zzz'",
            ),
            (
                json!({ "nodes": [{"id": "a", "agent": "general", "prompt": "p"}],
                    "conditional_edges": {"a": {"path": "{{nodes.a.outputs}}",
                        "map": {"ok": "END"}}} }),
                ".status or .failed_count",
            ),
            (
                json!({ "nodes": [{"id": "a", "agent": "general", "prompt": "p"}],
                    "conditional_edges": {"a": {"path": "{{nodes.a.status}}",
                        "map": {}}} }),
                "map must not be empty",
            ),
            (
                json!({ "nodes": [{"id": "a", "agent": "general", "prompt": "p"},
                    {"id": "b", "agent": "general", "prompt": "p"}],
                    "conditional_edges": {"a": {"path": "{{nodes.a.status}}",
                        "map": {"ok": "zzz"}}} }),
                "unknown node 'zzz'",
            ),
            (
                json!({ "nodes": [{"id": "a", "agent": "general", "prompt": "p"}],
                    "entry": "zzz" }),
                "unknown node 'zzz'",
            ),
        ];
        for (value, expected) in cases {
            let spec = spec(value);
            let err = spec.validate(BUILTINS).unwrap_err();
            assert!(err.contains(expected), "expected '{expected}' in '{err}'");
        }
    }

    #[test]
    fn validation_accepts_graph_features() {
        // fix is a router: its conditional map replaces its normal outgoing
        // edges, so it needs no explicit edge to combine.
        let spec = spec(json!({
            "nodes": [
                {"id": "read", "agent": "explore", "for_each": ["a", "b"], "prompt": "read {{item}} #{{index}}"},
                {"id": "check", "agent": "general", "for_each": {"from": "read"},
                 "prompt": "verify: {{item}}",
                 "review": {"agent": "general", "prompt": "{{output}} ok? item={{item}}", "pass_if": "PASS"}},
                {"id": "fix", "agent": "build", "prompt": "fix {{nodes.read.failed}}"},
                {"id": "combine", "agent": "general",
                 "prompt": "ok={{nodes.read.outputs}} bad={{nodes.read.failed_count}} status={{nodes.check.status}}"}
            ],
            "edges": [["read", "check"], ["check", "fix"], ["combine", "END"]],
            "conditional_edges": {"fix": {"path": "{{nodes.fix.status}}",
                "map": {"ok": "combine", "failed": "fix"}}}
        }));
        assert!(
            spec.validate(BUILTINS).is_ok(),
            "{:?}",
            spec.validate(BUILTINS)
        );
    }

    #[test]
    fn self_and_forward_references_are_declared() {
        // Templates may reference any declared node (loops re-read earlier
        // outcomes); the runtime renders an error for not-yet-run nodes.
        let spec = spec(json!({
            "nodes": [
                {"id": "a", "agent": "general", "prompt": "{{nodes.b.outputs}}"},
                {"id": "b", "agent": "general", "prompt": "{{nodes.b.failed_count}}"}
            ]
        }));
        assert!(spec.validate(BUILTINS).is_ok());
    }
}
