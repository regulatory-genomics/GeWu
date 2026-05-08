use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use serde_json::Value;

use crate::state::AppState;

pub async fn handle_plugin_get(
    State(app): State<Arc<AppState>>,
    Path((plugin_id, sidecar_id, path)): Path<(String, String, String)>,
    RawQuery(raw_query): RawQuery,
) -> Result<Response, (StatusCode, String)> {
    let target =
        plugin_target_url(&app, &plugin_id, &sidecar_id, &path, raw_query.as_deref()).await?;
    proxy_plugin_request(Method::GET, target, None).await
}

pub async fn handle_plugin_post(
    State(app): State<Arc<AppState>>,
    Path((plugin_id, sidecar_id, path)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> Result<Response, (StatusCode, String)> {
    let target = plugin_target_url(&app, &plugin_id, &sidecar_id, &path, None).await?;
    proxy_plugin_request(Method::POST, target, Some(body)).await
}

pub async fn plugin_target_url(
    app: &AppState,
    plugin_id: &str,
    sidecar_id: &str,
    path: &str,
    raw_query: Option<&str>,
) -> Result<String, (StatusCode, String)> {
    validate_proxy_path(path)?;
    let config = app.config.read().await;
    let Some(sidecars) = config.plugin_sidecars.get(plugin_id) else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!("plugin sidecars are not configured for {plugin_id}"),
        ));
    };
    let Some(base) = sidecars.get(sidecar_id) else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!("sidecar {sidecar_id} is not configured for plugin {plugin_id}"),
        ));
    };
    let mut target = format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    if let Some(query) = raw_query.filter(|query| !query.is_empty()) {
        target.push('?');
        target.push_str(query);
    }
    Ok(target)
}

fn validate_proxy_path(path: &str) -> Result<(), (StatusCode, String)> {
    if path.contains("..") || path.contains('\\') {
        return Err((
            StatusCode::BAD_REQUEST,
            "invalid plugin proxy path".to_string(),
        ));
    }
    Ok(())
}

async fn proxy_plugin_request(
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
            format!("plugin sidecar fetch failed: {e}"),
        )
    })?;
    let status = upstream.status();
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    let bytes = upstream.bytes().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("plugin sidecar response read failed: {e}"),
        )
    })?;

    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        content_type.unwrap_or_else(|| HeaderValue::from_static("application/json")),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_unsafe_proxy_paths() {
        assert!(validate_proxy_path("api/browser/samples").is_ok());
        assert!(validate_proxy_path("../secret").is_err());
        assert!(validate_proxy_path("api\\secret").is_err());
    }
}
