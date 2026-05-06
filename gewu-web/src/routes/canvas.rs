//! Canvas inverse channel.
//!
//! These endpoints let the browser-side canvas push state changes back to
//! the server. They are NOT tools — the agent calls `canvas.set_state`; the
//! frontend posts here. This split keeps the agent's audit trail (tool
//! calls) separate from raw user pans/zooms.
//!
//! Endpoints:
//!   - `GET    /api/sessions/:sid/canvas`               -> list
//!   - `GET    /api/sessions/:sid/canvas/:cid`          -> read
//!   - `POST   /api/sessions/:sid/canvas/:cid/state`    -> patch
//!   - `DELETE /api/sessions/:sid/canvas/:cid`          -> hide/minimize
//!
//! Auth is enforced by the route group's `auth_middleware::require_auth`
//! layer (cookie-based since Phase 2). Handlers do not re-validate.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::Json;
use gewu_canvas::CanvasStatePatch;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;

#[derive(Deserialize)]
pub struct PatchBody {
    pub patch: CanvasStatePatch,
}

pub async fn handle_list(
    State(app): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let canvases = app.canvases.list(&session_id).await;
    Ok(Json(json!({ "canvases": canvases })))
}

pub async fn handle_get(
    State(app): State<Arc<AppState>>,
    Path((session_id, canvas_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let state = app.canvases.get_state(&session_id, &canvas_id).await;
    match state {
        Some(s) => Ok(Json(json!({ "state": s }))),
        None => Err((StatusCode::NOT_FOUND, "canvas not found".to_string())),
    }
}

pub async fn handle_patch(
    State(app): State<Arc<AppState>>,
    Path((session_id, canvas_id)): Path<(String, String)>,
    Json(body): Json<PatchBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    match app
        .canvases
        .set_state(&session_id, &canvas_id, body.patch)
        .await
    {
        Ok(state) => Ok(Json(json!({ "state": state }))),
        Err(e) => Err((StatusCode::NOT_FOUND, e.to_string())),
    }
}

pub async fn handle_close(
    State(app): State<Arc<AppState>>,
    Path((session_id, canvas_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    match app.canvases.close(&session_id, &canvas_id).await {
        Ok(state) => Ok(Json(json!({ "closed": true, "state": state }))),
        Err(e) => Err((StatusCode::NOT_FOUND, e.to_string())),
    }
}

pub async fn handle_browser_get(
    State(app): State<Arc<AppState>>,
    Path(path): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Result<Response, (StatusCode, String)> {
    let target = browser_target_url(&app, &path, raw_query.as_deref()).await?;
    proxy_browser_request(Method::GET, target, None).await
}

pub async fn handle_browser_post(
    State(app): State<Arc<AppState>>,
    Path(path): Path<String>,
    Json(body): Json<Value>,
) -> Result<Response, (StatusCode, String)> {
    let target = browser_target_url(&app, &path, None).await?;
    proxy_browser_request(Method::POST, target, Some(body)).await
}

async fn browser_target_url(
    app: &AppState,
    path: &str,
    raw_query: Option<&str>,
) -> Result<String, (StatusCode, String)> {
    if path.contains("..") || path.contains('\\') {
        return Err((StatusCode::BAD_REQUEST, "invalid browser path".to_string()));
    }
    let config = app.config.read().await;
    let Some(base) = config.tachyon_browser_url.as_deref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "tachyon-browser is not configured".to_string(),
        ));
    };
    let mut target = format!("{}/api/browser/{}", base.trim_end_matches('/'), path);
    if let Some(query) = raw_query.filter(|query| !query.is_empty()) {
        target.push('?');
        target.push_str(query);
    }
    Ok(target)
}

async fn proxy_browser_request(
    method: Method,
    target: String,
    body: Option<Value>,
) -> Result<Response, (StatusCode, String)> {
    let client = reqwest::Client::new();
    let request = match method {
        Method::GET => client.get(&target),
        Method::POST => client.post(&target).json(&body.unwrap_or(Value::Null)),
        _ => {
            return Err((
                StatusCode::METHOD_NOT_ALLOWED,
                "unsupported method".to_string(),
            ))
        }
    };
    let upstream = request.send().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("tachyon-browser fetch failed: {e}"),
        )
    })?;
    let status = upstream.status();
    let bytes = upstream.bytes().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("tachyon-browser response read failed: {e}"),
        )
    })?;

    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(response)
}
