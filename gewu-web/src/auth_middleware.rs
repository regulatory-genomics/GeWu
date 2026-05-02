//! Auth middleware for the GeWu web server.
//!
//! Applied as an axum `route_layer` on the protected `/api/*` route group.
//! Single chokepoint: every request that lands on a protected route must
//! present `?token=<valid>` matching the in-process `AuthState`. Mismatch
//! returns 401 before the handler runs.
//!
//! The middleware reads the token from the URL query string only (matching
//! how the rest of the codebase passes it). Header-based tokens can be
//! added later; for MVP we keep the surface minimal.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};

use crate::state::AppState;

/// Reject requests that don't carry a valid `token` query param.
pub async fn require_token(
    State(app): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let query = req.uri().query().unwrap_or("");
    let token = parse_token_from_query(query);
    let auth = app.auth_token.read().await;
    let ok = token
        .as_deref()
        .map(|t| auth.validate(t))
        .unwrap_or(false);
    drop(auth);
    if !ok {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}

/// Pull `token=...` out of a query string. Returns the first match.
/// Decodes URL-encoded values (`+` and `%XX`).
fn parse_token_from_query(q: &str) -> Option<String> {
    for pair in q.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if k == "token" {
            return Some(decode_query(v));
        }
    }
    None
}

fn decode_query(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(h), Some(l)) =
                    (hex_nybble(bytes[i + 1]), hex_nybble(bytes[i + 2]))
                {
                    out.push((h << 4) | l);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

fn hex_nybble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_token_from_query() {
        assert_eq!(parse_token_from_query("token=abc"), Some("abc".to_string()));
        assert_eq!(
            parse_token_from_query("foo=1&token=xyz&bar=2"),
            Some("xyz".to_string())
        );
        assert_eq!(parse_token_from_query(""), None);
        assert_eq!(parse_token_from_query("foo=1"), None);
    }

    #[test]
    fn decodes_percent_encoded_token() {
        assert_eq!(parse_token_from_query("token=ab%2Bcd"), Some("ab+cd".to_string()));
    }
}
