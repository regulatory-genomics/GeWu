use super::graph_state::WorkflowState;
use super::spec::{ForEach, NodeSpec, ROUTE_DEFAULT, ROUTE_END, WorkflowSpec};
use super::template::{NodeTemplateData, TemplateVars, render};
use gewu_graph::graph::constants::END;
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PlannedEdge {
    Direct {
        source: String,
        target: String,
    },
    Join {
        sources: Vec<String>,
        target: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct PlannedRoute {
    pub source: String,
    pub path: String,
    pub map: BTreeMap<String, String>,
}

impl PlannedRoute {
    pub fn target(&self, state: &WorkflowState) -> String {
        let vars = TemplateVars {
            nodes: template_nodes(state),
            ..Default::default()
        };
        let Ok(key) = render(&self.path, &vars) else {
            return END.to_string();
        };
        self.map
            .get(&key)
            .or_else(|| self.map.get(ROUTE_DEFAULT))
            .cloned()
            .unwrap_or_else(|| END.to_string())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct GraphPlan {
    pub edges: Vec<PlannedEdge>,
    pub routes: Vec<PlannedRoute>,
    pub entry: String,
    pub max_steps: usize,
}

pub(super) fn plan_graph(spec: &WorkflowSpec) -> GraphPlan {
    let routers: HashSet<&str> = spec.conditional_edges.keys().map(String::as_str).collect();
    let mut by_target: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (from, to) in spec.resolved_edges() {
        if to == ROUTE_END || routers.contains(from.as_str()) {
            continue;
        }
        by_target.entry(to).or_default().push(from);
    }
    let edges = by_target
        .into_iter()
        .map(|(target, mut sources)| {
            sources.sort();
            sources.dedup();
            if sources.len() == 1 {
                PlannedEdge::Direct {
                    source: sources.remove(0),
                    target,
                }
            } else {
                PlannedEdge::Join { sources, target }
            }
        })
        .collect();
    let routes = spec
        .conditional_edges
        .iter()
        .map(|(source, route)| PlannedRoute {
            source: source.clone(),
            path: route.path.clone(),
            map: route
                .map
                .iter()
                .map(|(key, target)| {
                    (
                        key.clone(),
                        if target == ROUTE_END { END } else { target }.to_string(),
                    )
                })
                .collect(),
        })
        .collect();
    GraphPlan {
        edges,
        routes,
        entry: spec.entry_node().to_string(),
        max_steps: spec.steps(),
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct PlannedItem {
    pub item: Option<serde_json::Value>,
    pub prompt: Result<String, String>,
}

pub(super) fn plan_items(node: &NodeSpec, state: &WorkflowState) -> Vec<PlannedItem> {
    let items: Vec<Option<serde_json::Value>> = match &node.for_each {
        None => vec![None],
        Some(ForEach::Items(list)) => list.iter().cloned().map(Some).collect(),
        Some(ForEach::From { from }) => state
            .nodes
            .get(from)
            .map(|outcome| {
                outcome
                    .outputs
                    .iter()
                    .map(|output| Some(serde_json::Value::String(output.clone())))
                    .collect()
            })
            .unwrap_or_default(),
    };
    let nodes = template_nodes(state);
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let vars = TemplateVars {
                item: item.as_ref().map(render_item),
                index: node.for_each.is_some().then_some(index),
                output: None,
                nodes: nodes.clone(),
            };
            PlannedItem {
                item,
                prompt: render(&node.prompt, &vars)
                    .map_err(|error| format!("template error: {error}")),
            }
        })
        .collect()
}

fn template_nodes(state: &WorkflowState) -> BTreeMap<String, NodeTemplateData> {
    state
        .nodes
        .iter()
        .map(|(id, outcome)| (id.clone(), outcome.template_data()))
        .collect()
}

fn render_item(item: &serde_json::Value) -> String {
    match item {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::graph_state::{NodeOutcome, TaskFailure};
    use serde_json::json;

    fn spec() -> WorkflowSpec {
        serde_json::from_value(json!({"nodes": [
            {"id": "a", "agent": "build", "prompt": "p"},
            {"id": "b", "agent": "build", "prompt": "p"},
            {"id": "c", "agent": "build", "prompt": "p"}
        ]}))
        .unwrap()
    }

    fn direct(source: &str, target: &str) -> PlannedEdge {
        PlannedEdge::Direct {
            source: source.into(),
            target: target.into(),
        }
    }

    fn state() -> WorkflowState {
        WorkflowState {
            nodes: BTreeMap::from([(
                "a".into(),
                NodeOutcome {
                    outputs: vec!["first".into(), "second".into()],
                    failed: vec![TaskFailure {
                        index: 1,
                        item: json!(false),
                        error: "boom".into(),
                    }],
                    sessions: vec!["session".into()],
                },
            )]),
        }
    }

    #[test]
    fn absent_explicit_and_empty_edges() {
        let mut spec = spec();
        assert_eq!(
            plan_graph(&spec).edges,
            vec![direct("a", "b"), direct("b", "c")]
        );
        spec.edges = Some(vec![("a".into(), "c".into())]);
        spec.entry = Some("c".into());
        spec.max_steps = Some(3);
        let plan = plan_graph(&spec);
        assert_eq!(plan.edges, vec![direct("a", "c")]);
        assert_eq!(plan.entry, "c");
        assert_eq!(plan.max_steps, 3);
        spec.edges = Some(vec![]);
        assert!(plan_graph(&spec).edges.is_empty());
    }

    #[test]
    fn joins_deduplicate_sources_and_drop_end_edges() {
        let mut spec = spec();
        spec.edges = Some(vec![
            ("b".into(), "c".into()),
            ("a".into(), "c".into()),
            ("a".into(), "c".into()),
            ("a".into(), "b".into()),
            ("a".into(), "b".into()),
            ("c".into(), "END".into()),
        ]);
        assert_eq!(
            plan_graph(&spec).edges,
            vec![
                direct("a", "b"),
                PlannedEdge::Join {
                    sources: vec!["a".into(), "b".into()],
                    target: "c".into()
                }
            ]
        );
    }

    #[test]
    fn routes_replace_plain_edges_and_translate_targets() {
        let mut spec = spec();
        spec.conditional_edges.insert(
            "a".into(),
            serde_json::from_value(json!({
                "path": "{{nodes.a.failed_count}}", "map": {"1": "a", "0": "END", "*": "c"}
            }))
            .unwrap(),
        );
        let plan = plan_graph(&spec);
        assert_eq!(plan.edges, vec![direct("b", "c")]);
        let mut route = plan.routes.into_iter().next().unwrap();
        assert_eq!(route.source, "a");
        assert_eq!(route.map["0"], END);
        assert_eq!(route.target(&state()), "a");
        assert_eq!(route.target(&WorkflowState::default()), END);
        route.map.remove("1");
        assert_eq!(route.target(&state()), "c");
        route.map.insert("*".into(), END.into());
        assert_eq!(route.target(&state()), END);
        route.map.clear();
        assert_eq!(route.target(&state()), END);
        route.path = "{{".into();
        assert_eq!(route.target(&state()), END);
        spec.edges = Some(vec![("a".into(), "c".into()), ("b".into(), "c".into())]);
        assert_eq!(plan_graph(&spec).edges, vec![direct("b", "c")]);
    }

    #[test]
    fn single_and_empty_item_plans() {
        let mut node = spec().nodes.remove(0);
        assert_eq!(
            plan_items(&node, &state()),
            vec![PlannedItem {
                item: None,
                prompt: Ok("p".into())
            }]
        );
        for variable in ["item", "index", "output"] {
            node.prompt = format!("{{{{{variable}}}}}");
            assert_eq!(
                plan_items(&node, &state())[0].prompt,
                Err(format!(
                    "template error: {{{{{variable}}}}} has no value in this context"
                ))
            );
        }
        node.for_each = Some(ForEach::Items(vec![]));
        assert!(plan_items(&node, &state()).is_empty());
    }

    #[test]
    fn literal_items_preserve_values_and_render_in_order() {
        let mut node = spec().nodes.remove(0);
        let values = vec![
            json!("raw {{item}}"),
            json!({"x": 1}),
            json!([true, 2]),
            json!(null),
        ];
        node.for_each = Some(ForEach::Items(values.clone()));
        node.prompt = "{{index}}={{item}}".into();
        let plan = plan_items(&node, &state());
        for (index, value) in values.iter().enumerate() {
            assert_eq!(plan[index].item.as_ref(), Some(value));
        }
        assert_eq!(
            plan.iter()
                .map(|item| item.prompt.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["0=raw {{item}}", "1={\"x\":1}", "2=[true,2]", "3=null"]
        );
    }

    #[test]
    fn from_items_use_successful_outputs_and_node_template_data() {
        let mut node = spec().nodes.remove(1);
        node.for_each = Some(ForEach::From { from: "a".into() });
        node.prompt = "{{index}}={{item}} {{nodes.a.outputs}} {{nodes.a.status}} {{nodes.a.failed_count}} {{nodes.a.failed}}".into();
        let state = state();
        let plan = plan_items(&node, &state);
        assert_eq!(plan.len(), 2);
        for (index, output) in state.nodes["a"].outputs.iter().enumerate() {
            assert_eq!(plan[index].item, Some(json!(output)));
            assert_eq!(
                plan[index].prompt,
                Ok(format!(
                    "{index}={output} [\"first\",\"second\"] failed 1 {}",
                    json!([{"error": "boom", "index": 1, "item": false}])
                ))
            );
        }
        assert!(plan_items(&node, &WorkflowState::default()).is_empty());
        let mut empty = state;
        empty.nodes.get_mut("a").unwrap().outputs.clear();
        assert!(plan_items(&node, &empty).is_empty());
    }

    #[test]
    fn template_errors_preserve_each_item() {
        let mut node = spec().nodes.remove(0);
        node.for_each = Some(ForEach::Items(vec![json!("one"), json!(null)]));
        node.prompt = "{{nodes.a.outputs}}".into();
        let plan = plan_items(&node, &WorkflowState::default());
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].item, Some(json!("one")));
        assert_eq!(plan[1].item, Some(json!(null)));
        for item in plan {
            assert_eq!(
                item.prompt,
                Err("template error: node 'a' has not run yet (no outcome)".into())
            );
        }
    }
}
