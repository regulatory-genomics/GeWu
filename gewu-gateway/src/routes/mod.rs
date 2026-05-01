use axum::{
    Router,
    routing::{get, post},
};
use std::sync::Arc;

use crate::state::AppState;

pub mod catalog;
pub mod commands;
pub mod facts;
pub mod health;
pub mod inspector;
pub mod jobs;
pub mod runs;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/health", get(health::health))
        .route("/specialists", get(catalog::list_specialists))
        .route("/workflows", get(catalog::list_workflows))
        .route("/tools", get(catalog::list_tools))
        .route("/skills", get(catalog::list_skills))
        .route("/catalog/nodes", get(catalog::catalog_nodes))
        .route("/catalog/edges", get(catalog::catalog_edges))
        .route("/catalog/graph", get(catalog::catalog_graph))
        .route("/facts", get(facts::facts))
        .route("/commands", post(commands::execute_command))
        .route("/inspector/snapshot", get(inspector::inspector_snapshot))
        .route("/inspector/commands", post(inspector::inspector_commands))
        .route("/jobs", get(jobs::list_jobs).post(jobs::create_job))
        .route(
            "/jobs/{id}",
            get(jobs::get_job)
                .patch(jobs::update_job)
                .delete(jobs::delete_job),
        )
        .route("/jobs/{id}/run", post(jobs::run_job_now))
        .route("/runs", get(runs::list_runs))
        .route("/runs/{id}", get(runs::get_run))
        .route("/runs/{id}/logs", get(runs::get_run_logs))
        .route("/runs/{id}/events", get(runs::get_run_events))
}
