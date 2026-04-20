//! Workflow compiler: `WorkflowManifest` → `CompiledGraph<Blackboard>`.
//!
//! This is the bridge between the declarative manifest system and the
//! existing `StateGraph` engine. One file holds compiler, validator, and
//! predicate parser because they share small helper types and the whole
//! surface fits in under ~500 LOC.

use anyhow::{anyhow, Result};
use futures::future::join_all;
use gewu_core::types::{SpecialistExecutionRequest, ToolContext};
use gewu_graph::{Blackboard, CompiledGraph, StateGraph, END};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

use crate::manifest::workflow::{EdgeSpec, NodeSpec, WorkflowManifest};
use crate::runtime::GeWuRuntime;

type NodeFut = Pin<Box<dyn Future<Output = Blackboard> + Send>>;
type NodeFn = Arc<dyn Fn(Blackboard) -> NodeFut + Send + Sync>;

/// Compiles a `WorkflowManifest` into an executable `CompiledGraph<Blackboard>`.
///
/// Construct via `WorkflowCompiler::new(runtime).with_workflows(...)`. The
/// `workflows` map is consulted when resolving `kind: workflow` nodes (one
/// workflow referencing another).
pub struct WorkflowCompiler {
    runtime: Arc<GeWuRuntime>,
    workflows: Arc<HashMap<String, WorkflowManifest>>,
}

impl WorkflowCompiler {
    pub fn new(runtime: Arc<GeWuRuntime>) -> Self {
        Self {
            runtime,
            workflows: Arc::new(HashMap::new()),
        }
    }

    pub fn with_workflows(mut self, workflows: HashMap<String, WorkflowManifest>) -> Self {
        self.workflows = Arc::new(workflows);
        self
    }

    pub fn compile(&self, m: &WorkflowManifest) -> Result<CompiledGraph<Blackboard>> {
        validate(m, &self.runtime, &self.workflows)?;

        let session_id = format!("workflow:{}:{}", m.id, Uuid::new_v4());
        let mut graph: StateGraph<Blackboard> = StateGraph::new();
        graph.set_entry_point(&m.entry);
        for fin in &m.finish {
            graph.set_finish_point(fin);
        }

        for (node_id, spec) in &m.nodes {
            let handler = self.compile_node(node_id, spec, &session_id)?;
            let h = Arc::clone(&handler);
            graph.add_node(node_id.as_str(), move |bb: Blackboard| {
                let h = Arc::clone(&h);
                async move { h(bb).await }
            });
        }

        for edge in &m.edges {
            if let EdgeSpec::Plain([from, to]) = edge {
                graph.add_edge(from, to);
            }
        }

        let mut cond_by_source: HashMap<String, Vec<(Predicate, String)>> = HashMap::new();
        for edge in &m.edges {
            if let EdgeSpec::Conditional(c) = edge {
                let p = Predicate::parse(&c.when)?;
                cond_by_source
                    .entry(c.from.clone())
                    .or_default()
                    .push((p, c.to.clone()));
            }
        }
        for (source, preds) in cond_by_source {
            let preds_arc = Arc::new(preds);
            let preds_for_fn = Arc::clone(&preds_arc);
            let mut path_map: HashMap<String, String> = HashMap::new();
            for (_p, target) in preds_arc.iter() {
                path_map.insert(target.clone(), target.clone());
            }
            path_map.insert("__none__".to_string(), END.to_string());
            graph.add_conditional_edges(
                &source,
                move |bb: &Blackboard| {
                    for (p, target) in preds_for_fn.iter() {
                        if p.eval(bb) {
                            return target.clone();
                        }
                    }
                    "__none__".to_string()
                },
                path_map,
            );
        }

        Ok(graph.compile())
    }

    fn compile_node(&self, id: &str, spec: &NodeSpec, session_id: &str) -> Result<NodeFn> {
        match spec {
            NodeSpec::Specialist {
                specialist,
                tool,
                input,
                write,
                prompt,
            } => self.compile_specialist_node(
                id,
                specialist,
                tool.as_deref(),
                input.as_deref(),
                write.as_deref(),
                prompt.as_deref(),
                session_id,
            ),
            NodeSpec::Tool { tool, input, write } => self.compile_tool_node(
                id,
                tool,
                input.as_deref(),
                write.as_deref(),
                session_id,
            ),
            NodeSpec::Parallel { branches } => {
                self.compile_parallel_node(id, branches, session_id)
            }
            NodeSpec::Workflow {
                workflow,
                input,
                write,
            } => self.compile_workflow_node(
                id,
                workflow,
                input.as_deref(),
                write.as_deref(),
            ),
        }
    }

    fn compile_specialist_node(
        &self,
        id: &str,
        specialist: &str,
        tool_hint: Option<&str>,
        input: Option<&str>,
        write: Option<&str>,
        prompt: Option<&str>,
        session_id: &str,
    ) -> Result<NodeFn> {
        let runtime = Arc::clone(&self.runtime);
        let specialist = specialist.to_string();
        let tool_hint = tool_hint.map(|s| s.to_string());
        let input_path = input.map(|s| s.to_string());
        let write_path = write.map(|s| s.to_string()).unwrap_or_else(|| id.to_string());
        let prompt_template = prompt.map(|s| s.to_string());
        let session_id = session_id.to_string();
        let node_id = id.to_string();

        Ok(Arc::new(move |bb: Blackboard| -> NodeFut {
            let runtime = Arc::clone(&runtime);
            let specialist = specialist.clone();
            let tool_hint = tool_hint.clone();
            let input_path = input_path.clone();
            let write_path = write_path.clone();
            let prompt_template = prompt_template.clone();
            let session_id = session_id.clone();
            let node_id = node_id.clone();
            Box::pin(async move {
                let input_val = extract_input(&bb, input_path.as_deref());
                let prompt_text = build_prompt(
                    prompt_template.as_deref(),
                    tool_hint.as_deref(),
                    &input_val,
                );
                let task_id = format!("{}:{}", node_id, Uuid::new_v4());
                let req = SpecialistExecutionRequest {
                    task_id,
                    specialist_id: specialist.clone(),
                    session_id: format!("{}:{}", session_id, node_id),
                    prompt: prompt_text,
                    metadata_json: input_val,
                };
                let result = runtime
                    .execute_request(&req, Vec::new())
                    .await
                    .map(|e| e.output_json.unwrap_or(Value::Null))
                    .map_err(|e| e.to_string());
                to_partial(&write_path, result)
            })
        }))
    }

    fn compile_tool_node(
        &self,
        id: &str,
        cap: &str,
        input: Option<&str>,
        write: Option<&str>,
        session_id: &str,
    ) -> Result<NodeFn> {
        let runtime = Arc::clone(&self.runtime);
        let cap = cap.to_string();
        let input_path = input.map(|s| s.to_string());
        let write_path = write.map(|s| s.to_string()).unwrap_or_else(|| id.to_string());
        let session_id = session_id.to_string();
        let node_id = id.to_string();

        Ok(Arc::new(move |bb: Blackboard| -> NodeFut {
            let runtime = Arc::clone(&runtime);
            let cap = cap.clone();
            let input_path = input_path.clone();
            let write_path = write_path.clone();
            let session_id = session_id.clone();
            let node_id = node_id.clone();
            Box::pin(async move {
                let input_val = extract_input(&bb, input_path.as_deref());
                let tool = match runtime.resolve_capabilities(&[cap.clone()]) {
                    Ok(mut ts) => match ts.pop() {
                        Some(t) => t,
                        None => {
                            return error_partial(
                                &write_path,
                                format!("no tool produced for capability {}", cap),
                            );
                        }
                    },
                    Err(e) => return error_partial(&write_path, e.to_string()),
                };
                let ctx = ToolContext::new(session_id.clone(), node_id.clone());
                match tool.execute(input_val, &ctx).await {
                    Ok(tr) => {
                        let value = serde_json::from_str::<Value>(&tr.output)
                            .unwrap_or(Value::String(tr.output));
                        to_partial(&write_path, Ok(value))
                    }
                    Err(e) => error_partial(&write_path, e.to_string()),
                }
            })
        }))
    }

    fn compile_parallel_node(
        &self,
        _id: &str,
        branches: &BTreeMap<String, NodeSpec>,
        session_id: &str,
    ) -> Result<NodeFn> {
        let mut compiled: Vec<(String, NodeFn)> = Vec::new();
        for (branch_id, spec) in branches {
            let handler = self.compile_node(branch_id, spec, session_id)?;
            compiled.push((branch_id.clone(), handler));
        }
        let compiled = Arc::new(compiled);

        Ok(Arc::new(move |bb: Blackboard| -> NodeFut {
            let compiled = Arc::clone(&compiled);
            Box::pin(async move {
                let futures: Vec<_> = compiled
                    .iter()
                    .map(|(_, h)| {
                        let bb = bb.clone();
                        let h = Arc::clone(h);
                        async move { h(bb).await }
                    })
                    .collect();
                let results = join_all(futures).await;
                let mut merged = Blackboard::default();
                for partial in results {
                    merged.merge_in_place(partial);
                }
                merged
            })
        }))
    }

    fn compile_workflow_node(
        &self,
        id: &str,
        workflow_id: &str,
        input: Option<&str>,
        write: Option<&str>,
    ) -> Result<NodeFn> {
        let sub_manifest = self
            .workflows
            .get(workflow_id)
            .ok_or_else(|| anyhow!("Unknown subworkflow: {}", workflow_id))?
            .clone();
        let subgraph = self.compile(&sub_manifest)?;
        let input_path = input.map(|s| s.to_string());
        let write_path = write.map(|s| s.to_string()).unwrap_or_else(|| id.to_string());
        let subgraph = Arc::new(subgraph);

        Ok(Arc::new(move |bb: Blackboard| -> NodeFut {
            let subgraph = Arc::clone(&subgraph);
            let input_path = input_path.clone();
            let write_path = write_path.clone();
            Box::pin(async move {
                let input_val = extract_input(&bb, input_path.as_deref());
                let sub_bb = Blackboard::from_value(input_val);
                match subgraph.invoke(sub_bb).await {
                    Ok(out) => to_partial(&write_path, Ok(out.into_value())),
                    Err(e) => error_partial(&write_path, e.to_string()),
                }
            })
        }))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn extract_input(bb: &Blackboard, path: Option<&str>) -> Value {
    match path {
        None => bb.as_value().clone(),
        Some("$") => bb.as_value().clone(),
        Some(p) => {
            let p = p.strip_prefix("$.").unwrap_or(p);
            if p.is_empty() {
                bb.as_value().clone()
            } else {
                bb.get_path(p).cloned().unwrap_or(Value::Null)
            }
        }
    }
}

fn to_partial(write_path: &str, result: std::result::Result<Value, String>) -> Blackboard {
    match result {
        Ok(v) => {
            let mut bb = Blackboard::default();
            bb.set_path(write_path, v);
            bb
        }
        Err(e) => error_partial(write_path, e),
    }
}

fn error_partial(write_path: &str, msg: String) -> Blackboard {
    let mut bb = Blackboard::default();
    let key = write_path.replace('.', "_");
    bb.set_path(&format!("_errors.{}", key), Value::String(msg));
    bb
}

fn build_prompt(template: Option<&str>, tool_hint: Option<&str>, input: &Value) -> String {
    let input_str = match input {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        _ => serde_json::to_string_pretty(input).unwrap_or_else(|_| "{}".to_string()),
    };
    let base = match template {
        Some(t) => t.replace("{{input}}", &input_str),
        None if input_str.is_empty() => "Complete this workflow step.".to_string(),
        None => input_str,
    };
    match tool_hint {
        Some(t) => format!(
            "{}\n\nHint: the `{}` tool is available and relevant to this step.",
            base, t
        ),
        None => base,
    }
}

// ---------------------------------------------------------------------------
// Predicate parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Predicate {
    path: String,
    op: PredOp,
    rhs: Value,
}

#[derive(Debug, Clone)]
pub enum PredOp {
    Eq,
    Neq,
}

impl Predicate {
    pub fn parse(expr: &str) -> Result<Self> {
        let s = expr.trim();
        let (lhs, op, rhs) = if let Some(i) = s.find("==") {
            (s[..i].trim(), PredOp::Eq, s[i + 2..].trim())
        } else if let Some(i) = s.find("!=") {
            (s[..i].trim(), PredOp::Neq, s[i + 2..].trim())
        } else {
            return Err(anyhow!("Predicate must contain '==' or '!=': {}", expr));
        };
        let path = lhs.strip_prefix("$.").unwrap_or(lhs).to_string();
        if path.is_empty() {
            return Err(anyhow!("Predicate path is empty: {}", expr));
        }
        let rhs_val = parse_literal(rhs)?;
        Ok(Self {
            path,
            op,
            rhs: rhs_val,
        })
    }

    pub fn eval(&self, bb: &Blackboard) -> bool {
        let actual = bb.get_path(&self.path).cloned().unwrap_or(Value::Null);
        match self.op {
            PredOp::Eq => actual == self.rhs,
            PredOp::Neq => actual != self.rhs,
        }
    }
}

fn parse_literal(s: &str) -> Result<Value> {
    let s = s.trim();
    if s == "true" {
        return Ok(Value::Bool(true));
    }
    if s == "false" {
        return Ok(Value::Bool(false));
    }
    if s == "null" {
        return Ok(Value::Null);
    }
    serde_json::from_str::<Value>(s).map_err(|e| anyhow!("invalid literal `{}`: {}", s, e))
}

// ---------------------------------------------------------------------------
// Validator
// ---------------------------------------------------------------------------

pub fn validate(
    m: &WorkflowManifest,
    runtime: &GeWuRuntime,
    workflows: &HashMap<String, WorkflowManifest>,
) -> Result<()> {
    let specialists: HashSet<String> = runtime
        .list_specialists()
        .into_iter()
        .map(|s| s.name)
        .collect();
    let capabilities: HashSet<String> = runtime
        .capability_registry()
        .list_capabilities()
        .into_iter()
        .map(|c| c.name)
        .collect();
    validate_with_ids(m, &specialists, &capabilities, workflows)
}

/// Same validation as [`validate`], but parameterized by id sets instead of
/// a live runtime. Used by the composer's `write_workflow` tool so it can
/// validate a manifest without reaching back into the runtime.
pub fn validate_with_ids(
    m: &WorkflowManifest,
    known_specialists: &HashSet<String>,
    known_capabilities: &HashSet<String>,
    workflows: &HashMap<String, WorkflowManifest>,
) -> Result<()> {
    validate_refs_with_ids(m, known_specialists, known_capabilities, workflows)?;
    validate_structure(m)?;
    validate_predicates(m)?;
    validate_parallel_writes(m)?;
    Ok(())
}

fn validate_refs_with_ids(
    m: &WorkflowManifest,
    known_specialists: &HashSet<String>,
    known_capabilities: &HashSet<String>,
    workflows: &HashMap<String, WorkflowManifest>,
) -> Result<()> {
    for (id, spec) in &m.nodes {
        validate_node_refs_with_ids(id, spec, known_specialists, known_capabilities, workflows)?;
    }
    Ok(())
}

fn validate_node_refs_with_ids(
    id: &str,
    spec: &NodeSpec,
    known_specialists: &HashSet<String>,
    known_capabilities: &HashSet<String>,
    workflows: &HashMap<String, WorkflowManifest>,
) -> Result<()> {
    match spec {
        NodeSpec::Specialist { specialist, .. } => {
            if !known_specialists.contains(specialist) {
                return Err(anyhow!(
                    "node '{}' references unknown specialist: {}",
                    id,
                    specialist
                ));
            }
        }
        NodeSpec::Tool { tool, .. } => {
            if !known_capabilities.contains(tool) {
                return Err(anyhow!(
                    "node '{}' references unknown capability: {}",
                    id,
                    tool
                ));
            }
        }
        NodeSpec::Parallel { branches } => {
            for (bid, bspec) in branches {
                validate_node_refs_with_ids(
                    bid,
                    bspec,
                    known_specialists,
                    known_capabilities,
                    workflows,
                )?;
            }
        }
        NodeSpec::Workflow { workflow, .. } => {
            if !workflows.contains_key(workflow) {
                return Err(anyhow!(
                    "node '{}' references unknown workflow: {}",
                    id,
                    workflow
                ));
            }
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn validate_refs(
    m: &WorkflowManifest,
    runtime: &GeWuRuntime,
    workflows: &HashMap<String, WorkflowManifest>,
) -> Result<()> {
    for (id, spec) in &m.nodes {
        validate_node_refs(id, spec, runtime, workflows)?;
    }
    Ok(())
}

#[allow(dead_code)]
fn validate_node_refs(
    id: &str,
    spec: &NodeSpec,
    runtime: &GeWuRuntime,
    workflows: &HashMap<String, WorkflowManifest>,
) -> Result<()> {
    match spec {
        NodeSpec::Specialist { specialist, .. } => {
            if runtime.find_specialist(specialist).is_none() {
                return Err(anyhow!(
                    "node '{}' references unknown specialist: {}",
                    id,
                    specialist
                ));
            }
        }
        NodeSpec::Tool { tool, .. } => {
            if !runtime.capability_registry().contains(tool) {
                return Err(anyhow!(
                    "node '{}' references unknown capability: {}",
                    id,
                    tool
                ));
            }
        }
        NodeSpec::Parallel { branches } => {
            for (bid, bspec) in branches {
                validate_node_refs(bid, bspec, runtime, workflows)?;
            }
        }
        NodeSpec::Workflow { workflow, .. } => {
            if !workflows.contains_key(workflow) {
                return Err(anyhow!(
                    "node '{}' references unknown workflow: {}",
                    id,
                    workflow
                ));
            }
        }
    }
    Ok(())
}

fn validate_structure(m: &WorkflowManifest) -> Result<()> {
    if !m.nodes.contains_key(&m.entry) {
        return Err(anyhow!("entry '{}' is not a node", m.entry));
    }
    for fin in &m.finish {
        if !m.nodes.contains_key(fin) {
            return Err(anyhow!("finish '{}' is not a node", fin));
        }
    }
    for edge in &m.edges {
        if !m.nodes.contains_key(edge.from()) {
            return Err(anyhow!("edge from '{}' is not a node", edge.from()));
        }
        if edge.to() != END && !m.nodes.contains_key(edge.to()) {
            return Err(anyhow!("edge to '{}' is not a node", edge.to()));
        }
    }
    let mut reachable: HashSet<String> = HashSet::new();
    let mut stack = vec![m.entry.clone()];
    while let Some(n) = stack.pop() {
        if !reachable.insert(n.clone()) {
            continue;
        }
        for edge in &m.edges {
            if edge.from() == n && edge.to() != END {
                stack.push(edge.to().to_string());
            }
        }
    }
    for id in m.nodes.keys() {
        if !reachable.contains(id) {
            return Err(anyhow!("node '{}' is unreachable from entry", id));
        }
    }
    if !m.finish.is_empty() && !m.finish.iter().any(|f| reachable.contains(f)) {
        return Err(anyhow!("no finish node is reachable from entry"));
    }
    Ok(())
}

fn validate_predicates(m: &WorkflowManifest) -> Result<()> {
    for edge in &m.edges {
        if let EdgeSpec::Conditional(c) = edge {
            Predicate::parse(&c.when)?;
            if c.max_loops.unwrap_or(0) > 0 {
                return Err(anyhow!(
                    "max_loops > 0 not supported in v1 (edge {}→{})",
                    c.from,
                    c.to
                ));
            }
        }
    }
    Ok(())
}

fn validate_parallel_writes(m: &WorkflowManifest) -> Result<()> {
    for (id, spec) in &m.nodes {
        if let NodeSpec::Parallel { branches } = spec {
            let mut seen: HashSet<String> = HashSet::new();
            for (bid, bspec) in branches {
                let write = branch_write_key(bid, bspec);
                if !seen.insert(write.clone()) {
                    return Err(anyhow!(
                        "parallel node '{}' has duplicate branch write key '{}'",
                        id,
                        write
                    ));
                }
            }
        }
    }
    Ok(())
}

fn branch_write_key(bid: &str, spec: &NodeSpec) -> String {
    match spec {
        NodeSpec::Specialist { write, .. }
        | NodeSpec::Tool { write, .. }
        | NodeSpec::Workflow { write, .. } => write.clone().unwrap_or_else(|| bid.to_string()),
        NodeSpec::Parallel { .. } => bid.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn predicate_parses_and_evaluates_eq() {
        let p = Predicate::parse("$.review.needs_repair == true").unwrap();
        let mut bb = Blackboard::default();
        bb.set_path("review.needs_repair", json!(true));
        assert!(p.eval(&bb));
        bb.set_path("review.needs_repair", json!(false));
        assert!(!p.eval(&bb));
    }

    #[test]
    fn predicate_parses_string_literal() {
        let p = Predicate::parse(r#"$.status == "ok""#).unwrap();
        let mut bb = Blackboard::default();
        bb.set_path("status", json!("ok"));
        assert!(p.eval(&bb));
        bb.set_path("status", json!("err"));
        assert!(!p.eval(&bb));
    }

    #[test]
    fn predicate_parses_number_literal() {
        let p = Predicate::parse("$.count != 0").unwrap();
        let mut bb = Blackboard::default();
        bb.set_path("count", json!(3));
        assert!(p.eval(&bb));
        bb.set_path("count", json!(0));
        assert!(!p.eval(&bb));
    }

    #[test]
    fn predicate_rejects_malformed() {
        assert!(Predicate::parse("no operator here").is_err());
        assert!(Predicate::parse("$. == 1").is_err());
    }

    #[test]
    fn validate_rejects_unreachable_node() {
        let yaml = r#"
id: broken
entry: a
finish: [a]
nodes:
  a: { kind: tool, tool: t }
  orphan: { kind: tool, tool: t }
edges: []
"#;
        let m: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
        // structure-only check (no runtime)
        let err = validate_structure(&m).unwrap_err().to_string();
        assert!(err.contains("unreachable"), "got: {}", err);
    }

    #[test]
    fn validate_rejects_missing_entry_node() {
        let yaml = r#"
id: x
entry: nope
finish: []
nodes:
  a: { kind: tool, tool: t }
edges: []
"#;
        let m: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
        let err = validate_structure(&m).unwrap_err().to_string();
        assert!(err.contains("entry"), "got: {}", err);
    }

    #[test]
    fn validate_rejects_duplicate_parallel_write_keys() {
        let yaml = r#"
id: p
entry: p
finish: [p]
nodes:
  p:
    kind: parallel
    branches:
      a: { kind: tool, tool: t, write: shared }
      b: { kind: tool, tool: t, write: shared }
edges: []
"#;
        let m: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
        let err = validate_parallel_writes(&m).unwrap_err().to_string();
        assert!(err.contains("duplicate branch write key"), "got: {}", err);
    }

    #[test]
    fn validate_rejects_max_loops_over_zero_in_v1() {
        let yaml = r#"
id: l
entry: a
finish: [a]
nodes:
  a: { kind: tool, tool: t }
edges:
  - { from: a, to: a, when: "$.x == true", max_loops: 2 }
"#;
        let m: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
        let err = validate_predicates(&m).unwrap_err().to_string();
        assert!(err.contains("max_loops > 0"), "got: {}", err);
    }

    #[test]
    fn extract_input_dollar_path() {
        let mut bb = Blackboard::default();
        bb.set_path("tasks.bazi", json!({ "kind": "bazi" }));
        let v = extract_input(&bb, Some("$.tasks.bazi"));
        assert_eq!(v, json!({ "kind": "bazi" }));
    }

    #[test]
    fn extract_input_none_returns_full() {
        let mut bb = Blackboard::default();
        bb.set_path("x", json!(1));
        let v = extract_input(&bb, None);
        assert_eq!(v, json!({ "x": 1 }));
    }

    #[test]
    fn to_partial_writes_at_path() {
        let p = to_partial("runs.bazi", Ok(json!({ "claims": [] })));
        assert_eq!(p.get_path("runs.bazi"), Some(&json!({ "claims": [] })));
    }

    #[test]
    fn error_partial_writes_under_errors_namespace() {
        let p = error_partial("runs.bazi", "oops".into());
        assert_eq!(p.get_path("_errors.runs_bazi"), Some(&json!("oops")));
    }
}
