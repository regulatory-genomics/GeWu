//! Auth model for the GeWu web server.
//!
//! Phase 2 (C4) replaces the URL-token scheme with cookie-based auth.
//!
//! Modes:
//! - `AuthMode::None`: no auth at all. Used for the loopback default. The
//!   middleware short-circuits and lets every request through.
//! - `AuthMode::Password`: argon2-hashed password. Login posts the plaintext
//!   to `/api/login`, which verifies and issues an opaque session cookie.
//!   Subsequent requests are validated by the cookie store.
//!
//! There is no longer a `?token=` URL parameter. Cookies are sent
//! automatically by the browser for same-origin requests, including
//! `EventSource` SSE streams (which is why URL tokens were used historically;
//! cookies fix that without exposing secrets in URLs/logs).

use anyhow::Result;
use argon2::password_hash::SaltString;
use argon2::password_hash::rand_core::OsRng as PwOsRng;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use rand::RngCore;
use rand::rngs::OsRng;
use std::collections::HashSet;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::sync::RwLock;

const SESSION_TOKEN_BYTES: usize = 32;

#[derive(Clone)]
pub enum AuthMode {
    None,
    Password { argon2_hash: String },
}

impl AuthMode {
    pub fn from_cli(password: Option<&str>) -> Result<Self> {
        match password {
            None => Ok(Self::None),
            Some("") => Ok(Self::None),
            Some(plaintext) => {
                let salt = SaltString::generate(&mut PwOsRng);
                let hash = Argon2::default()
                    .hash_password(plaintext.as_bytes(), &salt)
                    .map_err(|e| anyhow::anyhow!("argon2 hash failed: {e}"))?
                    .to_string();
                Ok(Self::Password { argon2_hash: hash })
            }
        }
    }

    /// Verify a plaintext password against the configured hash. Returns false
    /// in `AuthMode::None` (open mode never accepts a "wrong" password — there
    /// is no password — but the login endpoint is also disabled in that mode).
    pub fn verify(&self, plaintext: &str) -> bool {
        match self {
            Self::None => false,
            Self::Password { argon2_hash } => {
                let parsed = match PasswordHash::new(argon2_hash) {
                    Ok(h) => h,
                    Err(_) => return false,
                };
                Argon2::default()
                    .verify_password(plaintext.as_bytes(), &parsed)
                    .is_ok()
            }
        }
    }

    pub fn requires_password(&self) -> bool {
        matches!(self, Self::Password { .. })
    }
}

/// In-memory store of live session cookies. Issuing a cookie inserts the
/// random token; logout removes it. Every request comparison is
/// constant-time.
#[derive(Default, Clone)]
pub struct CookieStore {
    live: Arc<RwLock<HashSet<String>>>,
}

impl CookieStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn issue(&self) -> String {
        let token = generate_session_token();
        self.live.write().await.insert(token.clone());
        token
    }

    pub async fn validate(&self, supplied: &str) -> bool {
        let live = self.live.read().await;
        for known in live.iter() {
            if known.as_bytes().ct_eq(supplied.as_bytes()).into() {
                return true;
            }
        }
        false
    }

    pub async fn revoke(&self, token: &str) {
        self.live.write().await.remove(token);
    }
}

fn generate_session_token() -> String {
    let mut bytes = [0u8; SESSION_TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Read the `gewu_session` cookie value out of an HTTP `Cookie` header value.
pub fn extract_session_cookie(header_value: &str) -> Option<&str> {
    for pair in header_value.split(';') {
        let pair = pair.trim();
        if let Some(rest) = pair.strip_prefix("gewu_session=") {
            return Some(rest);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_mode_none_for_no_password() {
        let mode = AuthMode::from_cli(None).unwrap();
        assert!(matches!(mode, AuthMode::None));
        assert!(!mode.verify("anything"));
        assert!(!mode.requires_password());
    }

    #[test]
    fn auth_mode_password_hashes_and_verifies() {
        let mode = AuthMode::from_cli(Some("hunter2")).unwrap();
        assert!(matches!(mode, AuthMode::Password { .. }));
        assert!(mode.verify("hunter2"));
        assert!(!mode.verify("wrong"));
        assert!(mode.requires_password());
    }

    #[tokio::test]
    async fn cookie_store_round_trip() {
        let store = CookieStore::new();
        let t = store.issue().await;
        assert!(store.validate(&t).await);
        assert!(!store.validate("not-a-token").await);
        store.revoke(&t).await;
        assert!(!store.validate(&t).await);
    }

    #[test]
    fn extracts_session_cookie() {
        assert_eq!(
            extract_session_cookie("gewu_session=abc; theme=dark"),
            Some("abc")
        );
        assert_eq!(extract_session_cookie("theme=dark"), None);
    }
}
