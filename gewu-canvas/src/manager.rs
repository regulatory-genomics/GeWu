//! In-memory canvas store.
//!
//! Per-session map of `canvas_id -> CanvasState`. MVP is in-memory only;
//! state survives the GeWu process lifetime but not restarts (matches
//! `AppState::active_turns`). A sqlite-backed variant lives in Phase 6 of
//! the implementation plan.

use std::collections::HashMap;
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::state::{CanvasKind, CanvasMode, CanvasState, CanvasStatePatch};

#[derive(Debug, Error)]
pub enum CanvasError {
    #[error("canvas {canvas_id} not found in session {session_id}")]
    NotFound {
        session_id: String,
        canvas_id: String,
    },
    #[error("invalid patch: {0}")]
    InvalidPatch(String),
}

/// Thread-safe handle around the canvas registry.
///
/// Cloning is cheap — wraps an `Arc`. Construct once in `AppState::new` and
/// pass clones to every canvas tool.
#[derive(Clone, Default)]
pub struct CanvasManager {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Default)]
struct Inner {
    /// session_id -> (canvas_id -> state)
    sessions: HashMap<String, HashMap<String, CanvasState>>,
}

impl CanvasManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create or focus a canvas. Returns the resulting state.
    ///
    /// If `canvas_id` is `Some` and already exists for this session, the
    /// existing state is returned with `focused = true` (and other fields
    /// updated from the inputs only when explicitly provided).
    pub async fn open(
        &self,
        session_id: &str,
        canvas_id: Option<String>,
        kind: CanvasKind,
        mode: CanvasMode,
        title: Option<String>,
        initial_payload: Option<serde_json::Value>,
    ) -> CanvasState {
        let mut guard = self.inner.write().await;
        let session = guard.sessions.entry(session_id.to_string()).or_default();

        if let Some(id) = canvas_id.as_deref() {
            if let Some(existing) = session.get_mut(id) {
                existing.focused = true;
                existing.visible = true;
                if let Some(t) = title {
                    existing.title = if t.is_empty() { None } else { Some(t) };
                }
                if let Some(p) = initial_payload {
                    existing.payload = p;
                }
                return existing.clone();
            }
        }

        // Defocus other canvases in the session — single-focus model for MVP.
        for state in session.values_mut() {
            state.focused = false;
        }

        let id = canvas_id.unwrap_or_else(|| short_canvas_id());
        let mut state = CanvasState::new(id.clone(), kind);
        state.mode = mode;
        if let Some(t) = title {
            state.title = if t.is_empty() { None } else { Some(t) };
        }
        if let Some(p) = initial_payload {
            state.payload = p;
        }
        session.insert(id, state.clone());
        state
    }

    pub async fn close(&self, session_id: &str, canvas_id: &str) -> Result<(), CanvasError> {
        let mut guard = self.inner.write().await;
        let session = guard
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| CanvasError::NotFound {
                session_id: session_id.into(),
                canvas_id: canvas_id.into(),
            })?;
        session
            .remove(canvas_id)
            .ok_or_else(|| CanvasError::NotFound {
                session_id: session_id.into(),
                canvas_id: canvas_id.into(),
            })?;
        Ok(())
    }

    pub async fn focus(&self, session_id: &str, canvas_id: &str) -> Result<CanvasState, CanvasError> {
        let mut guard = self.inner.write().await;
        let session = guard
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| CanvasError::NotFound {
                session_id: session_id.into(),
                canvas_id: canvas_id.into(),
            })?;
        if !session.contains_key(canvas_id) {
            return Err(CanvasError::NotFound {
                session_id: session_id.into(),
                canvas_id: canvas_id.into(),
            });
        }
        for (id, state) in session.iter_mut() {
            state.focused = id == canvas_id;
        }
        Ok(session.get(canvas_id).expect("checked above").clone())
    }

    pub async fn set_state(
        &self,
        session_id: &str,
        canvas_id: &str,
        patch: CanvasStatePatch,
    ) -> Result<CanvasState, CanvasError> {
        let mut guard = self.inner.write().await;
        let session = guard
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| CanvasError::NotFound {
                session_id: session_id.into(),
                canvas_id: canvas_id.into(),
            })?;
        let state = session.get_mut(canvas_id).ok_or_else(|| CanvasError::NotFound {
            session_id: session_id.into(),
            canvas_id: canvas_id.into(),
        })?;
        state.apply_patch(patch);
        Ok(state.clone())
    }

    pub async fn get_state(&self, session_id: &str, canvas_id: &str) -> Option<CanvasState> {
        let guard = self.inner.read().await;
        guard
            .sessions
            .get(session_id)
            .and_then(|m| m.get(canvas_id))
            .cloned()
    }

    pub async fn list(&self, session_id: &str) -> Vec<CanvasState> {
        let guard = self.inner.read().await;
        guard
            .sessions
            .get(session_id)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    pub async fn drop_session(&self, session_id: &str) {
        let mut guard = self.inner.write().await;
        guard.sessions.remove(session_id);
    }
}

fn short_canvas_id() -> String {
    let raw = Uuid::new_v4().to_string();
    let suffix: String = raw.chars().filter(|c| *c != '-').take(8).collect();
    format!("c-{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_then_get_returns_same_canvas() {
        let m = CanvasManager::new();
        let opened = m
            .open("s1", None, CanvasKind::Genome, CanvasMode::Overlay, None, None)
            .await;
        let fetched = m.get_state("s1", &opened.canvas_id).await.unwrap();
        assert_eq!(fetched.canvas_id, opened.canvas_id);
        assert!(fetched.focused);
        assert_eq!(fetched.kind, CanvasKind::Genome);
    }

    #[tokio::test]
    async fn open_with_known_id_focuses_existing() {
        let m = CanvasManager::new();
        m.open(
            "s1",
            Some("c-fixed".into()),
            CanvasKind::Genome,
            CanvasMode::Overlay,
            Some("first".into()),
            None,
        )
        .await;
        // Open another canvas to make first one lose focus
        m.open(
            "s1",
            Some("c-other".into()),
            CanvasKind::Genome,
            CanvasMode::Overlay,
            None,
            None,
        )
        .await;
        let first = m.get_state("s1", "c-fixed").await.unwrap();
        assert!(!first.focused);
        // Re-opening focuses it again, no duplicate created
        let reopened = m
            .open(
                "s1",
                Some("c-fixed".into()),
                CanvasKind::Genome,
                CanvasMode::Overlay,
                None,
                None,
            )
            .await;
        assert!(reopened.focused);
        assert_eq!(reopened.title.as_deref(), Some("first"));
        assert_eq!(m.list("s1").await.len(), 2);
    }

    #[tokio::test]
    async fn set_state_applies_patch_and_updates_timestamp() {
        let m = CanvasManager::new();
        let opened = m
            .open(
                "s1",
                None,
                CanvasKind::Genome,
                CanvasMode::Overlay,
                None,
                Some(serde_json::json!({"region": "chr1:1-100"})),
            )
            .await;
        let original_ts = opened.updated_at.clone();

        // Wait long enough for timestamp resolution to advance
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let patch: CanvasStatePatch = serde_json::from_value(serde_json::json!({
            "payload": { "region": "chr2:1-100" }
        }))
        .unwrap();
        let updated = m.set_state("s1", &opened.canvas_id, patch).await.unwrap();
        assert_eq!(updated.payload["region"], "chr2:1-100");
        assert_ne!(updated.updated_at, original_ts);
    }

    #[tokio::test]
    async fn set_state_unknown_canvas_returns_not_found() {
        let m = CanvasManager::new();
        let err = m
            .set_state("s1", "missing", CanvasStatePatch::default())
            .await
            .unwrap_err();
        assert!(matches!(err, CanvasError::NotFound { .. }));
    }

    #[tokio::test]
    async fn close_removes_canvas() {
        let m = CanvasManager::new();
        let opened = m
            .open("s1", None, CanvasKind::Genome, CanvasMode::Overlay, None, None)
            .await;
        m.close("s1", &opened.canvas_id).await.unwrap();
        assert!(m.get_state("s1", &opened.canvas_id).await.is_none());
    }

    #[tokio::test]
    async fn focus_makes_target_focused_and_others_not() {
        let m = CanvasManager::new();
        let a = m
            .open(
                "s1",
                Some("a".into()),
                CanvasKind::Genome,
                CanvasMode::Overlay,
                None,
                None,
            )
            .await;
        let b = m
            .open(
                "s1",
                Some("b".into()),
                CanvasKind::Genome,
                CanvasMode::Overlay,
                None,
                None,
            )
            .await;
        // After opening b, only b is focused
        assert!(!m.get_state("s1", &a.canvas_id).await.unwrap().focused);
        assert!(m.get_state("s1", &b.canvas_id).await.unwrap().focused);
        // Refocus a
        m.focus("s1", "a").await.unwrap();
        assert!(m.get_state("s1", "a").await.unwrap().focused);
        assert!(!m.get_state("s1", "b").await.unwrap().focused);
    }

    #[tokio::test]
    async fn drop_session_removes_all_canvases() {
        let m = CanvasManager::new();
        m.open("s1", None, CanvasKind::Genome, CanvasMode::Overlay, None, None)
            .await;
        m.drop_session("s1").await;
        assert!(m.list("s1").await.is_empty());
    }
}
