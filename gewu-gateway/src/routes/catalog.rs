use axum::{Json, extract::State};
use serde::Serialize;
use std::sync::Arc;

use crate::catalog::{CatalogGraph, CatalogIndexer, CatalogNode};
use crate::state::AppState;

#[derive(Serialize)]
pub struct SpecialistSummary {
    pub name: String,
    pub description: String,
}

#[derive(Serialize)]
pub struct WorkflowSummary {
    pub id: String,
    pub description: String,
    pub entry: String,
    pub finish: Vec<String>,
}

#[derive(Serialize)]
pub struct ToolSummary {
    pub name: String,
    pub description: String,
    pub provider_kind: String,
    pub input_schema: serde_json::Value,
    pub metadata: serde_json::Value,
}

#[derive(Serialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub specialists: Vec<String>,
    pub location: String,
}

pub async fn list_specialists(State(state): State<Arc<AppState>>) -> Json<Vec<SpecialistSummary>> {
    let specialists = state
        .runtime
        .list_specialists()
        .into_iter()
        .map(|agent| SpecialistSummary {
            name: agent.name,
            description: agent.description,
        })
        .collect();
    Json(specialists)
}

pub async fn list_workflows(State(state): State<Arc<AppState>>) -> Json<Vec<WorkflowSummary>> {
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
    Json(workflows)
}

pub async fn list_tools(State(state): State<Arc<AppState>>) -> Json<Vec<ToolSummary>> {
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
    Json(tools)
}

pub async fn list_skills(State(state): State<Arc<AppState>>) -> Json<Vec<SkillSummary>> {
    let skills = CatalogIndexer::new(state).load_skills();
    Json(skills)
}

pub async fn catalog_graph(State(state): State<Arc<AppState>>) -> Json<CatalogGraph> {
    Json(CatalogIndexer::new(state).build().await)
}

pub async fn catalog_nodes(State(state): State<Arc<AppState>>) -> Json<Vec<CatalogNode>> {
    Json(CatalogIndexer::new(state).build().await.nodes)
}

pub async fn catalog_edges(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<crate::catalog::CatalogEdge>> {
    Json(CatalogIndexer::new(state).build().await.edges)
}
