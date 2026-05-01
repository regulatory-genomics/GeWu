use axum::{
    Json,
    extract::{Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::inspector::commands::{InspectorCommandRequest, InspectorCommandResponse};
use crate::inspector::policy::ExposureLevel;
use crate::inspector::snapshot;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct SnapshotQuery {
    #[serde(default)]
    pub level: Option<String>,
}

pub async fn inspector_snapshot(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SnapshotQuery>,
    headers: HeaderMap,
) -> Json<snapshot::InspectorSnapshot> {
    let requested = ExposureLevel::from_query(query.level.as_deref());
    let effective = ExposureLevel::effective(requested, None, &headers);
    Json(snapshot::build_snapshot(state, effective).await)
}

pub async fn inspector_commands(
    State(state): State<Arc<AppState>>,
    Json(request): Json<InspectorCommandRequest>,
) -> Response {
    let response: InspectorCommandResponse =
        crate::inspector::commands::dispatch(state, request).await;
    Json(response).into_response()
}
