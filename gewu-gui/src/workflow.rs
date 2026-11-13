//! Workflow graph cards: parse the `workflow_start` spec JSON and the
//! `workflow_status`/`workflow_wait` snapshot text, then render the run's
//! topology as an inline SVG with per-node status colors.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Write as _;

use serde::Deserialize;
use serde::de::IgnoredAny;

use gewu_client::session::Block;

use crate::view::escape;
use crate::workspace::Tab;

// Geometry of the rendered graph (SVG user units).
const NODE_W: f32 = 148.0;
const NODE_H: f32 = 46.0;
const GAP_X: f32 = 62.0;
const GAP_Y: f32 = 24.0;
const PAD: f32 = 10.0;
const LOOP_DEPTH: f32 = 38.0;
const END_W: f32 = 54.0;
const END_H: f32 = 24.0;

/// The subset of the workflow spec the GUI needs to draw the graph.
#[derive(Debug, Deserialize, Default)]
pub struct WfSpec {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub nodes: Vec<WfNode>,
    #[serde(default)]
    pub edges: Option<Vec<(String, String)>>,
    #[serde(default)]
    pub conditional_edges: BTreeMap<String, WfRoute>,
    #[serde(default)]
    pub entry: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WfNode {
    pub id: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub for_each: Option<IgnoredAny>,
}

#[derive(Debug, Deserialize)]
pub struct WfRoute {
    #[serde(default)]
    pub map: BTreeMap<String, String>,
}

/// Per-node and run-level status parsed from a snapshot text.
#[derive(Debug, Default)]
pub struct WfSnapshot {
    pub run_status: Option<String>,
    pub done: Option<u32>,
    pub failed: Option<u32>,
    pub attempted: Option<u32>,
    pub current: Option<String>,
    pub nodes: BTreeMap<String, String>,
}

/// Parses the spec JSON out of a summarized `workflow_start` input. Tolerates
/// the bare spec, the full `{"spec": …}` wrapper, and a spec double-encoded
/// as a JSON string.
pub fn parse_spec(input: &str) -> Option<WfSpec> {
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    let mut value = value.get("spec").unwrap_or(&value).clone();
    if let serde_json::Value::String(encoded) = &value {
        value = serde_json::from_str(encoded).ok()?;
    }
    let spec: WfSpec = serde_json::from_value(value).ok()?;
    (!spec.nodes.is_empty()).then_some(spec)
}

/// Parses a run snapshot from tool output (status/wait/cancel). Returns `None`
/// for anything that is not a snapshot — start messages, errors, prose.
pub fn parse_snapshot(output: &str) -> Option<WfSnapshot> {
    let mut snap = WfSnapshot::default();
    let mut found = false;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("run ") {
            let Some((_, after)) = rest.split_once("): ") else {
                continue;
            };
            found = true;
            for (i, segment) in after.split(", ").enumerate() {
                if i == 0 {
                    snap.run_status = Some(segment.to_string());
                } else if let Some((count, tag)) = segment.split_once(' ')
                    && let Ok(count) = count.parse::<u32>()
                {
                    match tag {
                        "task(s) done" => snap.done = Some(count),
                        "failed" => snap.failed = Some(count),
                        "attempted" => snap.attempted = Some(count),
                        _ => {}
                    }
                }
            }
        } else if let Some(node) = line.strip_prefix("current node: ") {
            snap.current = Some(node.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("node ") {
            let Some((id, status)) = rest.split_once(": ") else {
                continue;
            };
            let status = status.split(", ").next().unwrap_or("").trim();
            if matches!(status, "ok" | "failed" | "pending" | "skipped") {
                snap.nodes.insert(id.trim().to_string(), status.to_string());
                found = true;
            }
        }
    }
    found.then_some(snap)
}

/// The run_id a `workflow_start` output reports, e.g.
/// `Started workflow run abc. It executes …` → `abc`.
fn start_run_id(output: &str) -> Option<String> {
    let rest = output.strip_prefix("Started workflow run ")?;
    let word = rest.split_whitespace().next()?;
    Some(word.trim_end_matches('.').to_string())
}

/// The run_id from a status/wait/cancel input: plain id (summarized) or the
/// original `{"run_id": …}` JSON (older sessions).
fn input_run_id(input: &str) -> Option<String> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(input) {
        return value
            .get("run_id")
            .and_then(|id| id.as_str())
            .map(str::to_string);
    }
    (!input.is_empty() && !input.contains(char::is_whitespace)).then(|| input.to_string())
}

/// Finds the spec of the run a status/wait/cancel call refers to by scanning
/// backwards for the `workflow_start` card that reported the same run_id.
fn spec_for_run(blocks: &[Block], run_id: &str) -> Option<WfSpec> {
    for block in blocks.iter().rev() {
        let Block::Tool {
            tool,
            input,
            output: Some(output),
        } = block
        else {
            continue;
        };
        if tool == "workflow_start" && start_run_id(output).as_deref() == Some(run_id) {
            return parse_spec(input);
        }
    }
    None
}

/// Compact card title for `workflow_start`: `name · N nodes`.
pub fn spec_title(input: &str) -> Option<String> {
    let spec = parse_spec(input)?;
    let nodes = spec.nodes.len();
    let plural = if nodes == 1 { "" } else { "s" };
    Some(match spec.name {
        Some(name) => format!("{name} · {nodes} node{plural}"),
        None => format!("{nodes} node{plural}"),
    })
}

/// SVG graph for a workflow snapshot card, or `None` when the card is not a
/// status/wait/cancel call or its data cannot be parsed (e.g. old sessions).
/// The `workflow_start` card stays plain: it only reports the run_id.
pub(crate) fn card_graph(
    tab: &Tab,
    tool: &str,
    input: &str,
    output: Option<&str>,
    index: usize,
) -> Option<String> {
    let spec = match tool {
        "workflow_status" | "workflow_wait" | "workflow_cancel" => {
            spec_for_run(&tab.model.blocks, &input_run_id(input)?)?
        }
        _ => return None,
    };
    let snapshot = output.and_then(parse_snapshot);
    Some(render_card(&spec, snapshot.as_ref(), index))
}

fn render_card(spec: &WfSpec, snapshot: Option<&WfSnapshot>, index: usize) -> String {
    let mut html = String::from("<div class=\"wf-card\">");
    if let Some(snap) = snapshot {
        html.push_str("<div class=\"wf-caption\">");
        if let Some(status) = &snap.run_status {
            let slug = status.replace(' ', "-");
            let _ = write!(
                html,
                "<span class=\"wf-run wf-run-{slug}\">{}</span> · ",
                escape(status)
            );
        }
        if let Some(done) = snap.done {
            let _ = write!(html, "{done} done");
            if let Some(failed) = snap.failed {
                let _ = write!(html, " · {failed} failed");
            }
            if let Some(attempted) = snap.attempted {
                let _ = write!(html, " · {attempted} attempted");
            }
            html.push_str(" · ");
        }
        match &snap.current {
            Some(node) => {
                let _ = write!(html, "current: {}", escape(&ellipsize(node, 24)));
            }
            None => html.push_str("no node running"),
        }
        html.push_str("</div>");
    }
    html.push_str(&graph_svg(spec, snapshot, index));
    html.push_str("</div>");
    html
}

#[derive(Clone)]
enum EdgeKind {
    Plain,
    Route(String),
}

impl EdgeKind {
    fn class(&self) -> &'static str {
        match self {
            EdgeKind::Plain => "wf-edge",
            EdgeKind::Route(_) => "wf-edge route",
        }
    }
    fn label(&self) -> Option<String> {
        match self {
            EdgeKind::Plain => None,
            EdgeKind::Route(label) => Some(escape(label)),
        }
    }
}

struct Layout {
    positions: Vec<(f32, f32)>,
    end: Option<(f32, f32)>,
    width: f32,
    height: f32,
    entry: usize,
    /// Forward edges between nodes.
    edges: Vec<(usize, usize, EdgeKind)>,
    /// Backward/self edges, routed below the graph.
    under: Vec<(usize, usize, EdgeKind)>,
    /// Sinks flow straight into END (always the rightmost column).
    end_straight: Vec<(usize, EdgeKind)>,
    /// Routes mapped to END arc below the graph so they never cut through
    /// intermediate nodes.
    end_under: Vec<(usize, EdgeKind)>,
}

fn layout(spec: &WfSpec) -> Layout {
    let n = spec.nodes.len();
    let index: HashMap<&str, usize> = spec
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id.as_str(), i))
        .collect();
    let has_route: Vec<bool> = spec
        .nodes
        .iter()
        .map(|node| spec.conditional_edges.contains_key(&node.id))
        .collect();

    // Plain edges: the explicit list, or the declaration-order chain; a
    // router replaces its node's plain outgoing edges (same as the compiler).
    let mut plain: Vec<(usize, usize)> = Vec::new();
    let pairs: Vec<(String, String)> = spec.edges.clone().unwrap_or_else(|| {
        (0..n.saturating_sub(1))
            .map(|i| (spec.nodes[i].id.clone(), spec.nodes[i + 1].id.clone()))
            .collect()
    });
    for (from, to) in pairs {
        let (Some(from), Some(to)) = (index.get(from.as_str()), index.get(to.as_str())) else {
            continue;
        };
        if has_route[*from] || plain.contains(&(*from, *to)) {
            continue;
        }
        plain.push((*from, *to));
    }

    // Route entries, merging repeated keys between the same pair into one
    // edge labeled `retry/*`.
    let mut routed: BTreeMap<(usize, usize), Vec<String>> = BTreeMap::new();
    let mut end_routed: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (source, route) in &spec.conditional_edges {
        let Some(&from) = index.get(source.as_str()) else {
            continue;
        };
        for (key, target) in &route.map {
            if target == "END" {
                end_routed.entry(from).or_default().push(key.clone());
            } else if let Some(&to) = index.get(target.as_str()) {
                routed.entry((from, to)).or_default().push(key.clone());
            }
        }
    }

    // Successor sets drive the layered layout.
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    let link = |succ: &mut Vec<Vec<usize>>, from: usize, to: usize| {
        if !succ[from].contains(&to) {
            succ[from].push(to);
        }
    };
    for &(from, to) in &plain {
        link(&mut succ, from, to);
    }
    for &(from, to) in routed.keys() {
        link(&mut succ, from, to);
    }

    // Edges into END: routes mapped to "END", plus sinks. END always sits one
    // column right of every node, so these are forward edges.
    let mut end_edges: Vec<(usize, EdgeKind)> = Vec::new();
    for (from, keys) in end_routed {
        end_edges.push((from, EdgeKind::Route(keys.join("/"))));
    }
    for (i, targets) in succ.iter().enumerate() {
        if targets.is_empty() {
            end_edges.push((i, EdgeKind::Plain));
        }
    }

    // BFS layers from the entry over successors; nodes unreachable from the
    // entry are parked one column past the reachable ones.
    let entry = spec
        .entry
        .as_deref()
        .and_then(|id| index.get(id).copied())
        .unwrap_or(0);
    let mut layer = vec![usize::MAX; n];
    layer[entry] = 0;
    let mut queue = VecDeque::from([(entry, 0usize)]);
    while let Some((node, depth)) = queue.pop_front() {
        for &to in &succ[node] {
            if layer[to] == usize::MAX {
                layer[to] = depth + 1;
                queue.push_back((to, depth + 1));
            }
        }
    }
    let reachable_max = layer.iter().copied().filter(|l| *l != usize::MAX).max();
    let parked = reachable_max.map_or(0, |max| max + 1);
    for l in &mut layer {
        if *l == usize::MAX {
            *l = parked;
        }
    }
    let last_layer = layer.iter().copied().max().unwrap_or(0);

    let column_count = last_layer + 1 + usize::from(!end_edges.is_empty());
    let mut columns: Vec<Vec<usize>> = vec![Vec::new(); column_count];
    for (i, l) in layer.iter().enumerate() {
        columns[*l].push(i);
    }
    let rows = columns[..last_layer + 1]
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(1);
    let row_step = NODE_H + GAP_Y;
    let total_h = rows as f32 * row_step - GAP_Y;

    let mut positions = vec![(0.0_f32, 0.0_f32); n];
    for (col, nodes) in columns[..last_layer + 1].iter().enumerate() {
        let x = PAD + col as f32 * (NODE_W + GAP_X);
        let offset = (total_h - (nodes.len() as f32 * row_step - GAP_Y)) / 2.0;
        for (row, &i) in nodes.iter().enumerate() {
            positions[i] = (x, PAD + offset + row as f32 * row_step);
        }
    }
    let end = (!end_edges.is_empty()).then(|| {
        (
            PAD + (last_layer + 1) as f32 * (NODE_W + GAP_X),
            PAD + (total_h - END_H) / 2.0,
        )
    });
    let width = PAD * 2.0 + column_count as f32 * (NODE_W + GAP_X) - GAP_X;

    // Split into forward and under (backward/self) edges, and END edges into
    // straight (sinks) and under (routes). Any under-route dips below the
    // graph, so reserve space for it once.
    let mut edges = Vec::new();
    let mut under = Vec::new();
    let mut place = |from: usize, to: usize, kind: EdgeKind| {
        if layer[to] < layer[from] || to == from {
            under.push((from, to, kind));
        } else {
            edges.push((from, to, kind));
        }
    };
    for (from, to) in plain {
        place(from, to, EdgeKind::Plain);
    }
    for ((from, to), keys) in routed {
        place(from, to, EdgeKind::Route(keys.join("/")));
    }
    let (end_straight, end_under): (Vec<_>, Vec<_>) = end_edges
        .into_iter()
        .partition(|(_, kind)| matches!(kind, EdgeKind::Plain));
    let mut height = PAD * 2.0 + total_h;
    if !under.is_empty() || !end_under.is_empty() {
        height += LOOP_DEPTH;
    }

    Layout {
        positions,
        end,
        width,
        height,
        entry,
        edges,
        under,
        end_straight,
        end_under,
    }
}

fn edge_path(x1: f32, y1: f32, x2: f32, y2: f32) -> String {
    let bend = GAP_X * 0.45;
    format!(
        "M {x1:.0} {y1:.0} C {:.0} {y1:.0}, {:.0} {y2:.0}, {x2:.0} {y2:.0}",
        x1 + bend,
        x2 - bend
    )
}

/// Path routed below the graph between two bottom anchor points, given as
/// horizontal centers. Self-loops exit right-of-center and re-enter left of
/// center so the path has length instead of collapsing to a point.
fn under_path(cx1: f32, y1_bottom: f32, cx2: f32, y2_bottom: f32) -> (String, f32) {
    let dip = y1_bottom.max(y2_bottom) + LOOP_DEPTH;
    let (sx, ex) = if (cx1 - cx2).abs() < f32::EPSILON {
        (cx1 + NODE_W * 0.22, cx2 - NODE_W * 0.22)
    } else {
        (cx1, cx2)
    };
    (
        format!(
            "M {sx:.0} {y1_bottom:.0} C {sx:.0} {dip:.0}, {ex:.0} {dip:.0}, {ex:.0} {y2_bottom:.0}"
        ),
        dip,
    )
}

fn status_class(snapshot: Option<&WfSnapshot>, id: &str) -> &'static str {
    match snapshot.and_then(|snap| snap.nodes.get(id).map(String::as_str)) {
        Some("ok") => " wf-ok",
        Some("failed") => " wf-failed",
        Some("skipped") => " wf-skipped",
        _ => " wf-pending",
    }
}

fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn graph_svg(spec: &WfSpec, snapshot: Option<&WfSnapshot>, uid: usize) -> String {
    let lay = layout(spec);
    let marker = format!("wfa{uid}");
    let mut svg = String::new();
    let _ = write!(
        svg,
        "<svg class=\"wf-svg\" style=\"max-width:{:.0}px\" viewBox=\"0 0 {:.0} {:.0}\" \
         role=\"img\" aria-label=\"workflow graph\">",
        lay.width, lay.width, lay.height
    );
    let _ = write!(
        svg,
        "<defs><marker id=\"{marker}\" class=\"wf-marker\" viewBox=\"0 0 8 8\" \
         refX=\"7\" refY=\"4\" markerWidth=\"7\" markerHeight=\"7\" orient=\"auto-start-reverse\">\
         <path d=\"M0 0 L8 4 L0 8 z\"/></marker></defs>"
    );

    svg.push_str("<g class=\"wf-edges\">");
    let mut labels: Vec<(f32, f32, String)> = Vec::new();
    for (from, to, kind) in &lay.edges {
        let (x1, y1) = lay.positions[*from];
        let (x2, y2) = lay.positions[*to];
        let ay1 = y1 + NODE_H / 2.0;
        let ay2 = y2 + NODE_H / 2.0;
        let _ = write!(
            svg,
            "<path class=\"{}\" marker-end=\"url(#{marker})\" d=\"{}\"/>",
            kind.class(),
            edge_path(x1 + NODE_W, ay1, x2, ay2)
        );
        if let Some(label) = kind.label() {
            labels.push(((x1 + NODE_W + x2) / 2.0, (ay1 + ay2) / 2.0 - 8.0, label));
        }
    }
    for (from, to, kind) in &lay.under {
        let (x1, y1) = lay.positions[*from];
        let (x2, y2) = lay.positions[*to];
        let (d, dip) = under_path(
            x1 + NODE_W / 2.0,
            y1 + NODE_H,
            x2 + NODE_W / 2.0,
            y2 + NODE_H,
        );
        let _ = write!(
            svg,
            "<path class=\"{}\" marker-end=\"url(#{marker})\" d=\"{d}\"/>",
            kind.class()
        );
        if let Some(label) = kind.label() {
            labels.push(((x1 + x2) / 2.0 + NODE_W / 2.0, dip - 6.0, label));
        }
    }
    if let Some((ex, ey)) = lay.end {
        for (from, kind) in &lay.end_straight {
            let (fx, fy) = lay.positions[*from];
            let ay = fy + NODE_H / 2.0;
            let _ = write!(
                svg,
                "<path class=\"{}\" marker-end=\"url(#{marker})\" d=\"{}\"/>",
                kind.class(),
                edge_path(fx + NODE_W, ay, ex, ey + END_H / 2.0)
            );
            if let Some(label) = kind.label() {
                labels.push((
                    (fx + NODE_W + ex) / 2.0,
                    (ay + ey + END_H / 2.0) / 2.0 - 8.0,
                    label,
                ));
            }
        }
        for (from, kind) in &lay.end_under {
            let (fx, fy) = lay.positions[*from];
            let (d, dip) = under_path(fx + NODE_W / 2.0, fy + NODE_H, ex + END_W / 2.0, ey + END_H);
            let _ = write!(
                svg,
                "<path class=\"{}\" marker-end=\"url(#{marker})\" d=\"{d}\"/>",
                kind.class()
            );
            if let Some(label) = kind.label() {
                labels.push((
                    (fx + NODE_W / 2.0 + ex + END_W / 2.0) / 2.0,
                    dip - 6.0,
                    label,
                ));
            }
        }
    }
    svg.push_str("</g>");

    svg.push_str("<g class=\"wf-labels\">");
    for (x, y, label) in labels {
        let _ = write!(
            svg,
            "<text class=\"wf-edge-label\" x=\"{x:.0}\" y=\"{y:.0}\">{label}</text>"
        );
    }
    svg.push_str("</g>");

    svg.push_str("<g class=\"wf-nodes\">");
    for (i, node) in spec.nodes.iter().enumerate() {
        let (x, y) = lay.positions[i];
        let mut class = format!("wf-node{}", status_class(snapshot, &node.id));
        if i == lay.entry {
            class.push_str(" wf-entry");
        }
        if snapshot.and_then(|s| s.current.as_deref()) == Some(node.id.as_str()) {
            class.push_str(" wf-current");
        }
        let mut agent = ellipsize(&node.agent, 20);
        if node.for_each.is_some() {
            agent.push_str(" · foreach");
        }
        let _ = write!(
            svg,
            "<g class=\"{class}\" transform=\"translate({x:.0} {y:.0})\">\
             <title>{}</title>\
             <rect width=\"{NODE_W}\" height=\"{NODE_H}\" rx=\"7\"/>\
             <text class=\"wf-id\" x=\"12\" y=\"19\">{}</text>\
             <text class=\"wf-agent\" x=\"12\" y=\"34\">{}</text></g>",
            escape(&node.id),
            escape(&ellipsize(&node.id, 18)),
            escape(&agent),
        );
    }
    if let Some((ex, ey)) = lay.end {
        let _ = write!(
            svg,
            "<g class=\"wf-end\" transform=\"translate({ex:.0} {ey:.0})\">\
             <rect width=\"{END_W}\" height=\"{END_H}\" rx=\"{}\"/>\
             <text x=\"{}\" y=\"16\">END</text></g>",
            END_H / 2.0,
            END_W / 2.0,
        );
    }
    svg.push_str("</g></svg>");
    svg
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"{
        "name": "docs",
        "nodes": [
            {"id": "read", "agent": "reader", "prompt": "p"},
            {"id": "polish", "agent": "writer", "prompt": "p",
             "for_each": ["a.md"], "review": {"agent": "rev", "prompt": "p"}},
            {"id": "combine", "agent": "writer", "prompt": "p"}
        ],
        "conditional_edges": {
            "polish": {"path": "{{nodes.polish.status}}",
                       "map": {"ok": "combine", "failed": "polish", "*": "END"}}
        },
        "entry": "read"
    }"#;

    const SNAPSHOT: &str = "run r-1 (docs): running, 3 task(s) done, 1 failed, 4 attempted\n\
         current node: polish\n\
         node read: ok, 2 done, 0 failed\n\
         \x20 agent sessions: s-0, s-1\n\
         node polish: failed, 1 done, 1 failed\n\
         \x20 task 0 failed (item: \"a.md\"): boom\n\
         latest node output:\npartial text\n\n\
         The run is still going. Call workflow_wait again to keep waiting.";

    #[test]
    fn parses_spec_from_wrapper_and_bare_form() {
        let wrapped = parse_spec(&format!(r#"{{"spec":{SPEC}}}"#)).unwrap();
        assert_eq!(wrapped.nodes.len(), 3);
        assert_eq!(wrapped.name.as_deref(), Some("docs"));
        assert!(wrapped.nodes[1].for_each.is_some());

        let bare = parse_spec(SPEC).unwrap();
        assert_eq!(bare.nodes[2].id, "combine");
        assert!(bare.conditional_edges.contains_key("polish"));
    }

    #[test]
    fn parses_a_string_encoded_spec() {
        let encoded = serde_json::to_string(&serde_json::json!({ "spec": SPEC })).unwrap();
        let wrapped = parse_spec(&encoded).unwrap();
        assert_eq!(wrapped.nodes.len(), 3);
        assert_eq!(wrapped.name.as_deref(), Some("docs"));
    }

    #[test]
    fn rejects_non_spec_input() {
        assert!(parse_spec("Started workflow run abc").is_none());
        assert!(parse_spec("").is_none());
        assert!(parse_spec(r#"{"nodes": []}"#).is_none());
    }

    #[test]
    fn parses_snapshot_text_and_ignores_prose() {
        let snap = parse_snapshot(SNAPSHOT).unwrap();
        assert_eq!(snap.run_status.as_deref(), Some("running"));
        assert_eq!(snap.done, Some(3));
        assert_eq!(snap.failed, Some(1));
        assert_eq!(snap.attempted, Some(4));
        assert_eq!(snap.current.as_deref(), Some("polish"));
        assert_eq!(snap.nodes.get("read").map(String::as_str), Some("ok"));
        assert_eq!(snap.nodes.get("polish").map(String::as_str), Some("failed"));
    }

    #[test]
    fn start_message_and_error_output_are_not_snapshots() {
        assert!(
            parse_snapshot("Started workflow run abc. It executes in the background.").is_none()
        );
        assert!(parse_snapshot("Error: unknown run: nope").is_none());
    }

    #[test]
    fn extracts_run_ids() {
        assert_eq!(
            start_run_id("Started workflow run abc-123. It executes in the background.").as_deref(),
            Some("abc-123")
        );
        assert_eq!(input_run_id("r-9").as_deref(), Some("r-9"));
        assert_eq!(input_run_id(r#"{"run_id": "r-9"}"#).as_deref(), Some("r-9"));
        assert!(input_run_id("not a run id with spaces").is_none());
    }

    #[test]
    fn spec_title_is_compact() {
        assert_eq!(spec_title(SPEC).as_deref(), Some("docs · 3 nodes"));
        assert!(spec_title("garbage").is_none());
    }

    #[test]
    fn graph_shows_topology_routes_and_status() {
        let spec = parse_spec(SPEC).unwrap();
        let snap = parse_snapshot(SNAPSHOT);

        let svg = graph_svg(&spec, snap.as_ref(), 7);
        assert!(svg.contains("max-width:") && svg.contains("viewBox"));
        // entry marker and node ids (read is the entry and already ok)
        assert!(svg.contains("wf-node wf-ok wf-entry"));
        assert!(svg.contains(">read</text>"));
        assert!(svg.contains(">polish</text>"));
        // router edges are dashed and labeled; the retry self-loop is under
        assert!(svg.contains("wf-edge route"));
        assert!(svg.contains(">ok</text>") && svg.contains(">failed</text>"));
        assert!(svg.contains("wf-labels"));
        // END pill from the `*` route and the combine sink
        assert!(svg.contains("wf-end"));
        // statuses overlay
        assert!(svg.contains("wf-ok") && svg.contains("wf-failed"));
        // unique marker id
        assert!(svg.contains("url(#wfa7)"));
        // foreach tag on the polish node
        assert!(svg.contains("foreach"));
    }

    #[test]
    fn chain_without_edges_becomes_linear_graph() {
        let spec = parse_spec(
            r#"{"nodes":[{"id":"a","agent":"x","prompt":"p"},{"id":"b","agent":"x","prompt":"p"}]}"#,
        )
        .unwrap();
        let svg = graph_svg(&spec, None, 1);
        // a → b chain edge plus b flowing into END
        assert_eq!(svg.matches("class=\"wf-edge\"").count(), 2);
        assert!(svg.contains("wf-entry"));
        // without a snapshot every node is pending
        assert_eq!(svg.matches("wf-pending").count(), 2);
    }

    #[test]
    fn graph_renders_card_html_with_caption() {
        let spec = parse_spec(SPEC).unwrap();
        let snap = parse_snapshot(SNAPSHOT);
        let html = render_card(&spec, snap.as_ref(), 3);
        assert!(html.starts_with("<div class=\"wf-card\">"));
        assert!(html.contains("wf-caption"));
        assert!(html.contains("wf-run-running"));
        assert!(html.contains("current: polish"));
        assert!(html.ends_with("</div>"));
    }
}
