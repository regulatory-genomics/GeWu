//! Login routes (Phase 2, C4 cookie auth).
//!
//! - `GET  /login`        — minimal HTML form posting to `/api/login`.
//! - `POST /api/login`    — verify password, issue cookie, return 204.
//! - `POST /api/logout`   — invalidate cookie.
//!
//! In `AuthMode::None` the login endpoint refuses (404) so users don't get a
//! confusing "wrong password" UX when no password is configured.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use crate::auth::extract_session_cookie;
use crate::state::AppState;

const LOGIN_HTML: &str = include_str!("login.html");

pub async fn login_page() -> Html<&'static str> {
    Html(LOGIN_HTML)
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

pub async fn handle_login(
    State(app): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Response {
    let mode = app.auth_mode.read().await.clone();
    if !mode.requires_password() {
        return (StatusCode::NOT_FOUND, "auth disabled").into_response();
    }
    if !mode.verify(&req.password) {
        return (StatusCode::UNAUTHORIZED, "invalid password").into_response();
    }

    let token = app.cookie_store.issue().await;
    let cookie =
        format!("gewu_session={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000");
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        header::HeaderValue::from_str(&cookie).expect("ascii cookie"),
    );
    (StatusCode::NO_CONTENT, headers).into_response()
}

pub async fn handle_logout(State(app): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(cookie_header) = headers.get(header::COOKIE)
        && let Ok(s) = cookie_header.to_str()
        && let Some(token) = extract_session_cookie(s)
    {
        app.cookie_store.revoke(token).await;
    }
    let mut out = HeaderMap::new();
    out.insert(
        header::SET_COOKIE,
        header::HeaderValue::from_static(
            "gewu_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        ),
    );
    (StatusCode::NO_CONTENT, out).into_response()
}

/// Returns whether the server is in password mode. Public — frontend uses it
/// to decide whether to show the login redirect.
pub async fn handle_auth_mode(State(app): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let requires = app.auth_mode.read().await.requires_password();
    Json(serde_json::json!({ "password_required": requires }))
}
