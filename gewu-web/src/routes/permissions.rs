use crate::state::AppState;
use axum::{Json, extract::State};
use gewu_core::permission::{
    GrantDuration, PermissionDecisionKind, PermissionGrant, ToolPermissionRequest,
};
use std::sync::Arc;

#[derive(Debug, serde::Deserialize)]
pub struct PermissionDecisionRequest {
    pub request: ToolPermissionRequest,
    pub decision: PermissionDecisionKind,
    #[serde(default)]
    pub duration: Option<GrantDuration>,
}

#[derive(Debug, serde::Serialize)]
pub struct PermissionDecisionResponse {
    pub request_id: String,
    pub approved: bool,
    pub grant_count: usize,
}

pub async fn handle_permission_decision(
    State(app): State<Arc<AppState>>,
    Json(req): Json<PermissionDecisionRequest>,
) -> Result<Json<PermissionDecisionResponse>, axum::http::StatusCode> {
    let approved = req.decision == PermissionDecisionKind::Approve;
    let decision = app
        .resolve_permission_decision(&req.request, req.decision.clone(), req.duration)
        .await;
    if approved {
        let grant = PermissionGrant {
            session_id: req.request.session_id.clone(),
            action: req.request.action.clone(),
            scope: req.request.scope.clone(),
            duration: decision.duration,
        };
        app.add_permission_grant(grant)
            .await
            .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    let grants = app
        .permission_grants_for_session(&req.request.session_id)
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(PermissionDecisionResponse {
        request_id: req.request.id,
        approved,
        grant_count: grants.len(),
    }))
}
