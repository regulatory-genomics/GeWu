use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode},
    routing::get,
};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

async fn test_app() -> Router {
    let dir = tempfile::tempdir().unwrap().keep();
    let config = gewu_web::state::ServerConfig {
        api_key: Some("test-key".to_string()),
        endpoint: Some("http://127.0.0.1:9/v1".to_string()),
        db_path: Some(dir.join("gewu.sqlite").to_string_lossy().to_string()),
        ..gewu_web::state::ServerConfig::default()
    };
    let state = Arc::new(
        gewu_web::state::AppState::new(
            config,
            gewu_web::auth::AuthMode::None,
            Vec::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap(),
    );

    Router::new()
        .route(
            "/api/workspaces/{workspace_id}/mailbox",
            get(gewu_web::routes::mailbox::poll_mailbox)
                .post(gewu_web::routes::mailbox::post_mailbox),
        )
        .with_state(state)
}

#[tokio::test]
async fn mailbox_post_and_poll() {
    let app = test_app().await;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/workspaces/default/mailbox")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"type": "table.row_selected", "node_id": "n1", "row_id": "r1"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["ids"].as_array().unwrap().len(), 1);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/workspaces/default/mailbox")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    assert_eq!(body["events"].as_array().unwrap().len(), 1);
    assert_eq!(body["events"][0]["event"]["type"], "table.row_selected");

    let cursor = body["cursor"].as_str().unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/workspaces/default/mailbox?since={cursor}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    assert_eq!(body["events"].as_array().unwrap().len(), 0);
}
