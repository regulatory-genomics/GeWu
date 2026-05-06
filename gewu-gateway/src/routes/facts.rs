use axum::{Json, extract::State};
use serde::Serialize;
use std::sync::Arc;

use crate::catalog::CatalogIndexer;
use crate::routes::catalog::{SkillSummary, SpecialistSummary, ToolSummary, WorkflowSummary};
use crate::scheduler::job::{JobRun, ScheduledJob};
use crate::state::AppState;

#[derive(Serialize)]
pub struct GatewayFact {
    pub service: &'static str,
    pub version: &'static str,
    pub model: String,
    pub db_path: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FactsResponse {
    pub gateway: GatewayFact,
    pub jobs: Vec<ScheduledJob>,
    pub runs: Vec<JobRun>,
    pub specialists: Vec<SpecialistSummary>,
    pub workflows: Vec<WorkflowSummary>,
    pub sops: Vec<serde_json::Value>,
    pub tools: Vec<ToolSummary>,
    pub skills: Vec<SkillSummary>,
    pub runtime_plugins: Vec<serde_json::Value>,
}

pub async fn facts(State(state): State<Arc<AppState>>) -> Json<FactsResponse> {
    let specialists = state
        .runtime
        .list_specialists()
        .into_iter()
        .map(|agent| SpecialistSummary {
            name: agent.name,
            description: agent.description,
        })
        .collect();

    let workflows = state
        .workflow_registry()
        .map(|registry| {
            let mut workflows: Vec<WorkflowSummary> = registry
                .as_map()
                .values()
                .map(|workflow| WorkflowSummary {
                    id: workflow.id.clone(),
                    description: workflow.description.clone(),
                    entry: workflow.entry.clone(),
                    finish: workflow.finish.clone(),
                })
                .collect();
            workflows.sort_by(|left, right| left.id.cmp(&right.id));
            workflows
        })
        .unwrap_or_default();

    let sops = state
        .runtime
        .sop_registry()
        .list()
        .into_iter()
        .map(|sop| serde_json::to_value(sop).unwrap_or_default())
        .collect();

    let tools = state
        .runtime
        .capability_registry()
        .list_capabilities()
        .into_iter()
        .map(|capability| ToolSummary {
            name: capability.name,
            description: capability.description,
            provider_kind: capability.provider_kind,
            input_schema: capability.input_schema,
            metadata: capability.metadata,
        })
        .collect();

    let indexer = CatalogIndexer::new(state.clone());
    Json(FactsResponse {
        gateway: GatewayFact {
            service: "gewu-gateway",
            version: env!("CARGO_PKG_VERSION"),
            model: state.config.model.clone(),
            db_path: state.config.db_path.display().to_string(),
        },
        jobs: state.jobs.list_jobs().await.unwrap_or_default(),
        runs: state.jobs.list_runs().await.unwrap_or_default(),
        specialists,
        workflows,
        sops,
        tools,
        skills: indexer.load_skills(),
        runtime_plugins: indexer.load_runtime_plugins(),
    })
}
