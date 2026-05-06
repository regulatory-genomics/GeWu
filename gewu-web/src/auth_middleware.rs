//! Auth middleware for the GeWu web server.
//!
//! Phase 2 (C4): cookie-based auth replacing URL tokens.
//!
//! - `AuthMode::None`: every request passes through.
//! - `AuthMode::Password`: requests must carry a `gewu_session` cookie that
//!   matches the in-process `CookieStore`. Mismatch returns 401 for `/api/*`
//!   (handlers expect JSON responses) and a 302 redirect to `/login` for
//!   anything else (so unauthenticated browser navigation lands on the
//!   login page).

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::header,
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};

use crate::auth::{extract_session_cookie, AuthMode};
use crate::state::AppState;

pub async fn require_auth(
    State(app): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let mode = app.auth_mode.read().await.clone();
    if matches!(mode, AuthMode::None) {
        return next.run(req).await;
    }

    let path = req.uri().path().to_string();
    let token_opt = req
        .headers()
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| extract_session_cookie(s).map(|t| t.to_string()));
    let valid = match token_opt {
        Some(t) => app.cookie_store.validate(&t).await,
        None => false,
    };

    let _ = &mut req; // borrow check appeasement; req is still owned
    if valid {
        return next.run(req).await;
    }
    if path.starts_with("/api/") {
        (axum::http::StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    } else {
        Redirect::to("/login").into_response()
    }
}
