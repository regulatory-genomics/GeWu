use serde::Serialize;
use std::sync::Arc;

use crate::catalog::CatalogIndexer;
use crate::inspector::policy::ExposureLevel;
use crate::inspector::redact;
use crate::state::AppState;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectorSnapshot {
    pub summary: SnapshotSummary,
    pub nodes: Vec<InspectorNode>,
    pub edges: Vec<InspectorEdge>,
    pub jobs: Vec<serde_json::Value>,
    pub runs: Vec<serde_json::Value>,
    pub specialists: Vec<serde_json::Value>,
    pub sops: Vec<serde_json::Value>,
    pub workflows: Vec<serde_json::Value>,
    pub tools: Vec<serde_json::Value>,
    pub skills: Vec<serde_json::Value>,
    pub runtime_plugins: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSummary {
    pub service: &'static str,
    pub version: &'static str,
    pub exposure_level: ExposureLevel,
    pub node_count: usize,
    pub edge_count: usize,
    pub job_count: usize,
    pub run_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectorNode {
    pub id: String,
    pub kind: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub badges: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<NodeSource>,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeSource {
    pub kind: String,
    pub path: String,
    pub redacted: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectorEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub label: String,
    pub provenance: String,
}

pub async fn build_snapshot(state: Arc<AppState>, level: ExposureLevel) -> InspectorSnapshot {
    let indexer = CatalogIndexer::new(state.clone());
    let catalog = indexer.build().await;

    let jobs = state.jobs.list_jobs().await.unwrap_or_default();
    let runs = state.jobs.list_runs().await.unwrap_or_default();
    let skills = indexer.load_skills();
    let runtime_plugins = indexer.load_runtime_plugins();
    let sops: Vec<serde_json::Value> = state
        .runtime
        .sop_registry()
        .list()
        .into_iter()
        .map(|sop| serde_json::to_value(sop).unwrap_or_default())
        .collect();

    let mut nodes: Vec<InspectorNode> = catalog
        .nodes
        .iter()
        .map(|cn| {
            let source = cn.source_path.as_ref().map(|p| NodeSource {
                kind: "manifest".to_string(),
                path: redact::redact_path(p, level),
                redacted: !level.allows_source_paths(),
            });
            InspectorNode {
                id: cn.id.clone(),
                kind: cn.node_type.clone(),
                label: cn.name.clone(),
                description: cn.description.clone(),
                capabilities: json_string_array(&cn.metadata, "capabilities"),
                skills: json_string_array(&cn.metadata, "skills"),
                badges: Vec::new(),
                source,
                details: if level.allows_source_paths() || cn.node_type == "specialist" {
                    cn.metadata.clone()
                } else {
                    serde_json::Value::Null
                },
            }
        })
        .collect();

    let edges: Vec<InspectorEdge> = catalog
        .edges
        .iter()
        .map(|ce| InspectorEdge {
            from: ce.from.clone(),
            to: ce.to.clone(),
            kind: ce.edge_type.clone(),
            label: ce.edge_type.replace('_', " "),
            provenance: "catalog".to_string(),
        })
        .collect();

    let run_edges: Vec<InspectorEdge> = runs
        .iter()
        .map(|run| InspectorEdge {
            from: format!("run:{}", run.id),
            to: format!("job:{}", run.job_id),
            kind: "execution_of".to_string(),
            label: "execution of".to_string(),
            provenance: "job_store".to_string(),
        })
        .collect();

    let run_nodes: Vec<InspectorNode> = runs
        .iter()
        .map(|run| InspectorNode {
            id: format!("run:{}", run.id),
            kind: "run".to_string(),
            label: format!("{} ({})", run.job_id, run.status),
            description: Some(run.status.clone()),
            capabilities: Vec::new(),
            skills: Vec::new(),
            badges: vec![run.status.clone()],
            source: None,
            details: serde_json::json!({
                "started_at": run.started_at,
                "finished_at": run.finished_at,
                "duration_ms": run.duration_ms,
            }),
        })
        .collect();

    nodes.extend(run_nodes);
    let mut all_edges = edges;
    all_edges.extend(run_edges);

    let jobs_json: Vec<serde_json::Value> = jobs
        .iter()
        .map(|j| {
            serde_json::json!({
                "id": j.id,
                "name": j.name,
                "enabled": j.enabled,
                "schedule": j.schedule,
                "timezone": j.timezone,
                "specialist_id": j.specialist_id,
                "prompt": redact::redact_prompt(&j.prompt, level),
                "no_overlap": j.no_overlap,
                "next_run_at": j.next_run_at,
            })
        })
        .collect();

    let runs_json: Vec<serde_json::Value> = runs
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "job_id": r.job_id,
                "status": r.status,
                "started_at": r.started_at,
                "finished_at": r.finished_at,
                "duration_ms": r.duration_ms,
                "prompt": r.prompt.as_deref().map(|p| redact::redact_prompt(p, level)),
                "output": r.output.as_deref().map(|o| redact::redact_output(o, level)),
                "error": r.error.as_deref().map(|e| redact::redact_error(e, level)),
            })
        })
        .collect();

    let specialists_json: Vec<serde_json::Value> = state
        .runtime
        .list_specialist_descriptors()
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "name": s.id,
                "description": s.description,
                "capabilities": s.capabilities,
                "skills": s.skills,
            })
        })
        .collect();

    let workflows_json: Vec<serde_json::Value> = state
        .workflow_registry()
        .map(|reg| {
            reg.as_map()
                .values()
                .map(|w| {
                    serde_json::json!({
                        "id": w.id,
                        "description": w.description,
                        "entry": w.entry,
                        "finish": w.finish,
                        "nodes": if level.allows_source_paths() {
                            serde_json::to_value(&w.nodes).unwrap_or_default()
                        } else {
                            serde_json::json!(w.nodes.keys().collect::<Vec<_>>())
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let tools_json: Vec<serde_json::Value> = state
        .runtime
        .capability_registry()
        .list_capabilities()
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "provider_kind": t.provider_kind,
            })
        })
        .collect();

    let skills_json: Vec<serde_json::Value> = skills
        .iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "description": s.description,
                "capabilities": s.capabilities,
                "specialists": s.specialists,
                "location": redact::redact_path(&s.location, level),
            })
        })
        .collect();

    let node_count = nodes.len();
    let edge_count = all_edges.len();

    InspectorSnapshot {
        summary: SnapshotSummary {
            service: "gewu-gateway",
            version: env!("CARGO_PKG_VERSION"),
            exposure_level: level,
            node_count,
            edge_count,
            job_count: jobs.len(),
            run_count: runs_json.len(),
        },
        nodes,
        edges: all_edges,
        jobs: jobs_json,
        runs: runs_json,
        specialists: specialists_json,
        sops,
        workflows: workflows_json,
        tools: tools_json,
        skills: skills_json,
        runtime_plugins,
    }
}

fn json_string_array(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}
