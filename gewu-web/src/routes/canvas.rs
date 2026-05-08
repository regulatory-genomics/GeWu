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

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use gewu_canvas::CanvasStatePatch;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;

#[derive(Deserialize)]
pub struct PatchBody {
    pub patch: CanvasStatePatch,
}

pub async fn handle_list(
    State(app): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let session_id = app
        .resolve_session_id(&session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let canvases = app.canvases.list(&session_id).await;
    Ok(Json(json!({ "canvases": canvases })))
}

pub async fn handle_get(
    State(app): State<Arc<AppState>>,
    Path((session_id, canvas_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let session_id = app
        .resolve_session_id(&session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
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
    let session_id = app
        .resolve_session_id(&session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
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
    let session_id = app
        .resolve_session_id(&session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
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
            ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use std::sync::Arc;

    /// Verifies that canvas routes resolve session_id prefixes correctly.
    ///
    /// Setup:
    /// 1. Store a session with full UUID in the session_store
    /// 2. Open a canvas under the full session_id
    /// 3. Call handle_list with a short prefix of the session_id
    /// 4. Assert the canvas appears in the list
    #[tokio::test]
    async fn canvas_list_resolves_session_prefix() {
        let config = crate::state::ServerConfig {
            api_key: Some("test-key".to_string()),
            endpoint: Some("http://localhost".to_string()),
            model: Some("test".to_string()),
            ..Default::default()
        };

        let app = AppState::new(
            config,
            crate::auth::AuthMode::None,
            vec![],
            Arc::new(|_| {}),
        )
        .await
        .expect("AppState should be created");

        let app = Arc::new(app);

        let full_session_id = uuid::Uuid::new_v4().to_string();
        app.session_store
            .insert(&full_session_id, None, None, "test-agent", 1000)
            .await
            .expect("session should be inserted");

        // Open a canvas under the full session_id
        let canvas_state = app
            .canvases
            .open(
                &full_session_id,
                Some("test-canvas".to_string()),
                gewu_canvas::CanvasKind::Genome,
                gewu_canvas::CanvasMode::Overlay,
                Some("Test Canvas".to_string()),
                None,
            )
            .await
            .expect("canvas should open");

        // Call handle_list with a short prefix - this is the key test:
        // the handler should resolve "48995146" to "48995146-9994-4ca5-923c-e90ca3afa9dd"
        let prefix = &full_session_id[..8];
        let result = handle_list(State(app.clone()), Path(prefix.to_string())).await;

        // Assert the result contains the canvas we opened
        let Ok(Json(response)) = result else {
            panic!("handle_list failed: {:?}", result);
        };

        let canvases = response["canvases"]
            .as_array()
            .expect("response should have canvases array");

        assert!(
            !canvases.is_empty(),
            "canvas list should not be empty when using session prefix"
        );
        assert!(
            canvases.iter().any(|c| c["canvas_id"] == canvas_state.canvas_id),
            "opened canvas should appear in list when queried with session prefix"
        );
    }
}
