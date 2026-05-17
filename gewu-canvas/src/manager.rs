//! In-memory canvas store.
//!
//! Per-session map of `canvas_id -> CanvasState`. MVP is in-memory only;
//! state survives the GeWu process lifetime but not restarts (matches
//! `AppState::active_turns`). A sqlite-backed variant lives in Phase 6 of
//! the implementation plan.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::state::{
    CanvasKind, CanvasMode, CanvasState, CanvasStatePatch, normalize_signal_source,
};

#[derive(Debug, Error)]
pub enum CanvasError {
    #[error("canvas {canvas_id} not found in session {session_id}")]
    NotFound {
        session_id: String,
        canvas_id: String,
    },
    #[error("invalid patch: {0}")]
    InvalidPatch(String),
    #[error("invalid genome canvas payload")]
    InvalidGenomeCanvasPayload(Vec<CanvasValidationIssue>),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CanvasValidationIssue {
    pub path: String,
    pub code: &'static str,
    pub message: String,
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
    /// updated from the inputs only when explicitly provided). The
    /// single-focus invariant is enforced in both branches: any other
    /// canvas in the session is defocused.
    pub async fn open(
        &self,
        session_id: &str,
        canvas_id: Option<String>,
        kind: CanvasKind,
        mode: CanvasMode,
        title: Option<String>,
        initial_payload: Option<serde_json::Value>,
    ) -> Result<CanvasState, CanvasError> {
        let canvas_id = normalize_canvas_id(canvas_id);
        let mut guard = self.inner.write().await;
        let session = guard.sessions.entry(session_id.to_string()).or_default();

        if let Some(id) = canvas_id.as_deref()
            && let Some(existing) = session.get(id)
        {
            let mut candidate = existing.clone();
            candidate.visible = true;
            candidate.focused = true;
            if let Some(t) = title {
                candidate.title = if t.is_empty() { None } else { Some(t) };
            }
            if let Some(p) = initial_payload {
                candidate.payload = p;
                candidate.validate_payload()?;
            }

            // Defocus everything else first so we never end up with
            // two focused canvases after a re-open.
            for (other_id, state) in session.iter_mut() {
                state.focused = other_id == id;
            }
            let existing = session
                .get_mut(id)
                .ok_or_else(|| CanvasError::NotFound {
                    session_id: session_id.into(),
                    canvas_id: id.to_string(),
                })?;
            *existing = candidate;
            return Ok(existing.clone());
        }

        // New canvas: defocus existing canvases (single-focus model).
        for state in session.values_mut() {
            state.focused = false;
        }

        let id = canvas_id.unwrap_or_else(short_canvas_id);
        let mut state = CanvasState::new(id.clone(), kind);
        state.mode = mode;
        if let Some(t) = title {
            state.title = if t.is_empty() { None } else { Some(t) };
        }
        if let Some(p) = initial_payload {
            state.payload = p;
            state.validate_payload()?;
        }
        session.insert(id, state.clone());
        Ok(state)
    }

    pub async fn close(
        &self,
        session_id: &str,
        canvas_id: &str,
    ) -> Result<CanvasState, CanvasError> {
        let mut guard = self.inner.write().await;
        let session = guard
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| CanvasError::NotFound {
                session_id: session_id.into(),
                canvas_id: canvas_id.into(),
            })?;
        let state = session
            .get_mut(canvas_id)
            .ok_or_else(|| CanvasError::NotFound {
                session_id: session_id.into(),
                canvas_id: canvas_id.into(),
            })?;
        state.visible = false;
        state.focused = false;
        Ok(state.clone())
    }

    pub async fn focus(
        &self,
        session_id: &str,
        canvas_id: &str,
    ) -> Result<CanvasState, CanvasError> {
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
        let mut focused = None;
        for (id, state) in session.iter_mut() {
            state.focused = id == canvas_id;
            if state.focused {
                focused = Some(state.clone());
            }
        }
        focused.ok_or_else(|| CanvasError::NotFound {
            session_id: session_id.into(),
            canvas_id: canvas_id.into(),
        })
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
        let state = session
            .get_mut(canvas_id)
            .ok_or_else(|| CanvasError::NotFound {
                session_id: session_id.into(),
                canvas_id: canvas_id.into(),
            })?;
        if let Some(payload_patch) = &patch.payload {
            let mut candidate = state.clone();
            candidate.apply_patch(CanvasStatePatch {
                payload: Some(payload_patch.clone()),
                ..Default::default()
            });
            candidate.validate_payload()?;
        }
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

impl CanvasState {
    pub fn validate_payload(&self) -> Result<(), CanvasError> {
        validate_payload(self.kind, &self.payload)
    }
}

fn validate_payload(kind: CanvasKind, payload: &serde_json::Value) -> Result<(), CanvasError> {
    if kind != CanvasKind::Genome {
        return Ok(());
    }
    let mut issues = Vec::new();
    let Some(object) = payload.as_object() else {
        issues.push(CanvasValidationIssue {
            path: "payload".to_string(),
            code: "invalid_type",
            message: "genome canvas payload must be an object".to_string(),
        });
        return Err(CanvasError::InvalidGenomeCanvasPayload(issues));
    };

    match object.get("assembly").and_then(serde_json::Value::as_str) {
        Some(assembly) if !assembly.trim().is_empty() => {}
        _ => issues.push(CanvasValidationIssue {
            path: "assembly".to_string(),
            code: "missing_required",
            message: "genome canvas payload requires non-empty assembly".to_string(),
        }),
    }

    match object.get("region").and_then(serde_json::Value::as_str) {
        Some(region) if !region.trim().is_empty() => {}
        _ => issues.push(CanvasValidationIssue {
            path: "region".to_string(),
            code: "missing_required",
            message: "genome canvas payload requires non-empty region".to_string(),
        }),
    }

    let sample_ids = validate_samples(object.get("samples"), &mut issues);
    validate_tracks(object.get("tracks"), &sample_ids, &mut issues);

    if issues.is_empty() {
        Ok(())
    } else {
        Err(CanvasError::InvalidGenomeCanvasPayload(issues))
    }
}

fn validate_samples(
    samples: Option<&serde_json::Value>,
    issues: &mut Vec<CanvasValidationIssue>,
) -> HashSet<String> {
    let mut sample_ids = HashSet::new();
    let Some(samples) = samples else {
        return sample_ids;
    };
    let Some(samples) = samples.as_array() else {
        issues.push(CanvasValidationIssue {
            path: "samples".to_string(),
            code: "invalid_type",
            message: "samples must be an array".to_string(),
        });
        return sample_ids;
    };

    for (index, sample) in samples.iter().enumerate() {
        let path = format!("samples[{index}]");
        let Some(sample) = sample.as_object() else {
            issues.push(CanvasValidationIssue {
                path,
                code: "invalid_type",
                message: "sample must be an object".to_string(),
            });
            continue;
        };
        match sample.get("id").and_then(serde_json::Value::as_str) {
            Some(id) if !id.trim().is_empty() => {
                sample_ids.insert(id.to_string());
            }
            _ => issues.push(CanvasValidationIssue {
                path: format!("{path}.id"),
                code: "missing_required",
                message: "sample requires non-empty id".to_string(),
            }),
        }
        if sample.contains_key("sample_id") {
            issues.push(CanvasValidationIssue {
                path: format!("{path}.sample_id"),
                code: "unexpected_field",
                message: "samples use canonical id, not sample_id".to_string(),
            });
        }
    }
    sample_ids
}

fn validate_tracks(
    tracks: Option<&serde_json::Value>,
    sample_ids: &HashSet<String>,
    issues: &mut Vec<CanvasValidationIssue>,
) {
    let Some(tracks) = tracks else {
        return;
    };
    let Some(tracks) = tracks.as_array() else {
        issues.push(CanvasValidationIssue {
            path: "tracks".to_string(),
            code: "invalid_type",
            message: "tracks must be an array".to_string(),
        });
        return;
    };

    for (index, track) in tracks.iter().enumerate() {
        let path = format!("tracks[{index}]");
        let Some(track) = track.as_object() else {
            issues.push(CanvasValidationIssue {
                path,
                code: "invalid_type",
                message: "track must be an object".to_string(),
            });
            continue;
        };
        if let Some(sample_id) = track.get("sample_id").and_then(serde_json::Value::as_str)
            && !sample_id.trim().is_empty()
            && !sample_ids.contains(sample_id)
        {
            issues.push(CanvasValidationIssue {
                path: format!("{path}.sample_id"),
                code: "unknown_reference",
                message: format!("track references unknown sample_id '{sample_id}'"),
            });
        }
        if track
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|track_type| track_type == "signal")
            && let Some(source) = track.get("source").and_then(serde_json::Value::as_str)
        {
            match normalize_signal_source(Some(source)) {
                Ok(normalized) if normalized == source.trim().to_ascii_lowercase() => {}
                Ok(normalized) => issues.push(CanvasValidationIssue {
                    path: format!("{path}.source"),
                    code: "source_invalid",
                    message: format!(
                        "raw genome payload source '{source}' is not canonical; use '{normalized}'"
                    ),
                }),
                Err(message) => issues.push(CanvasValidationIssue {
                    path: format!("{path}.source"),
                    code: "source_invalid",
                    message,
                }),
            }
        }
    }
}

fn short_canvas_id() -> String {
    let raw = Uuid::new_v4().to_string();
    let suffix: String = raw.chars().filter(|c| *c != '-').take(8).collect();
    format!("c-{suffix}")
}

fn normalize_canvas_id(canvas_id: Option<String>) -> Option<String> {
    canvas_id.and_then(|id| {
        let id = id.trim();
        if id.is_empty() {
            None
        } else {
            Some(id.to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_then_get_returns_same_canvas() {
        let m = CanvasManager::new();
        let opened = m
            .open(
                "s1",
                None,
                CanvasKind::Genome,
                CanvasMode::Overlay,
                None,
                None,
            )
            .await
            .unwrap();
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
        .await
        .unwrap();
        // Open another canvas to make first one lose focus
        m.open(
            "s1",
            Some("c-other".into()),
            CanvasKind::Genome,
            CanvasMode::Overlay,
            None,
            None,
        )
        .await
        .unwrap();
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
            .await
            .unwrap();
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
                Some(serde_json::json!({"assembly": "hg38", "region": "chr1:1-100"})),
            )
            .await
            .unwrap();
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
    async fn close_hides_canvas_without_removing_it() {
        let m = CanvasManager::new();
        let opened = m
            .open(
                "s1",
                None,
                CanvasKind::Genome,
                CanvasMode::Overlay,
                None,
                None,
            )
            .await
            .unwrap();
        let closed = m.close("s1", &opened.canvas_id).await.unwrap();
        assert!(!closed.visible);
        assert!(!closed.focused);
        let stored = m.get_state("s1", &opened.canvas_id).await.unwrap();
        assert_eq!(stored.canvas_id, opened.canvas_id);
        assert!(!stored.visible);
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
            .await
            .unwrap();
        let b = m
            .open(
                "s1",
                Some("b".into()),
                CanvasKind::Genome,
                CanvasMode::Overlay,
                None,
                None,
            )
            .await
            .unwrap();
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
        m.open(
            "s1",
            None,
            CanvasKind::Genome,
            CanvasMode::Overlay,
            None,
            None,
        )
        .await
        .unwrap();
        m.drop_session("s1").await;
        assert!(m.list("s1").await.is_empty());
    }

    /// Regression: re-opening an existing canvas must defocus its siblings.
    /// Originally the code only set focused=true on the target without
    /// clearing focus on others, so the single-focus invariant could be
    /// broken (issue surfaced in code review).
    #[tokio::test]
    async fn reopening_existing_canvas_defocuses_siblings() {
        let m = CanvasManager::new();
        // Open A (auto-focused), then B (auto-focused — A loses focus).
        let a = m
            .open(
                "s1",
                Some("a".into()),
                CanvasKind::Genome,
                CanvasMode::Overlay,
                None,
                None,
            )
            .await
            .unwrap();
        let b = m
            .open(
                "s1",
                Some("b".into()),
                CanvasKind::Genome,
                CanvasMode::Overlay,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(!m.get_state("s1", &a.canvas_id).await.unwrap().focused);
        assert!(m.get_state("s1", &b.canvas_id).await.unwrap().focused);

        // Re-open A. B must lose focus; only A is focused after.
        let reopened = m
            .open(
                "s1",
                Some("a".into()),
                CanvasKind::Genome,
                CanvasMode::Overlay,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(reopened.focused);
        assert!(!m.get_state("s1", "b").await.unwrap().focused);
        let focused: Vec<_> = m
            .list("s1")
            .await
            .into_iter()
            .filter(|s| s.focused)
            .collect();
        assert_eq!(focused.len(), 1, "exactly one canvas may be focused");
        assert_eq!(focused[0].canvas_id, "a");
    }

    // -------------------------------------------------------------------------
    // genome_canvas_payload_builder tests
    // -------------------------------------------------------------------------

    use crate::state::{
        GenomeCanvasOpenRequest, GenomeCanvasSampleInput, GenomeCanvasTrackInput, TrackType,
        build_genome_canvas_payload,
    };

    #[test]
    fn genome_canvas_builder_creates_canonical_ucf_spi1_payload() {
        let request = GenomeCanvasOpenRequest {
            assembly: "hg38".to_string(),
            region: "chr11:47354860-47395607".to_string(),
            gene_symbol: Some("SPI1".to_string()),
            samples: vec![GenomeCanvasSampleInput {
                id: "UCF".to_string(),
                label: Some("UCF".to_string()),
                assay: Some("atac".to_string()),
                uri: Some("gewu://sample/ucf_atac_hg38".to_string()),
            }],
            tracks: vec![GenomeCanvasTrackInput {
                id: "signal-atac-UCF".to_string(),
                track_type: TrackType::Signal,
                source: Some("coverage".to_string()),
                sample_id: "UCF".to_string(),
                assay: Some("atac".to_string()),
                uri: Some("gewu://track/ucf_atac_signal_hg38".to_string()),
                path: Some(
                    "/data2/litian/macrophage_bk/data/ATAC/ucf/P1-2_ATAC3.filtered.ucf".to_string(),
                ),
                url: Some(
                    "/data2/litian/macrophage_bk/data/ATAC/ucf/P1-2_ATAC3.filtered.ucf".to_string(),
                ),
            }],
        };

        let payload = build_genome_canvas_payload(request).expect("should build payload");

        // Verify top-level fields
        assert_eq!(payload["assembly"], "hg38");
        assert_eq!(payload["region"], "chr11:47354860-47395607");
        assert_eq!(payload["gene"]["symbol"], "SPI1");
        assert_eq!(payload["gene"]["region"], "chr11:47354860-47395607");

        // Verify samples[0]
        let sample = &payload["samples"][0];
        assert_eq!(sample["id"], "UCF");
        assert_eq!(sample["label"], "UCF");
        assert_eq!(sample["assay"], "atac");
        assert_eq!(sample["uri"], "gewu://sample/ucf_atac_hg38");
        // Samples must NOT have sample_id
        assert!(
            !sample.as_object().unwrap().contains_key("sample_id"),
            "sample should not have sample_id field"
        );

        // Verify tracks[0]
        let track = &payload["tracks"][0];
        assert_eq!(track["id"], "signal-atac-UCF");
        assert_eq!(track["type"], "signal");
        assert_eq!(track["source"], "coverage");
        assert_eq!(track["sample_id"], "UCF");
        assert_eq!(track["assay"], "atac");
        assert_eq!(track["uri"], "gewu://track/ucf_atac_signal_hg38");
        assert_eq!(
            track["path"],
            "/data2/litian/macrophage_bk/data/ATAC/ucf/P1-2_ATAC3.filtered.ucf"
        );
        assert_eq!(
            track["url"],
            "/data2/litian/macrophage_bk/data/ATAC/ucf/P1-2_ATAC3.filtered.ucf"
        );
        assert_eq!(track["visible"], true);
    }

    #[test]
    fn genome_canvas_builder_rejects_track_unknown_sample() {
        let request = GenomeCanvasOpenRequest {
            assembly: "hg38".to_string(),
            region: "chr1:1-100".to_string(),
            gene_symbol: None,
            samples: vec![], // No samples
            tracks: vec![GenomeCanvasTrackInput {
                id: "track1".to_string(),
                track_type: TrackType::Signal,
                source: None,
                sample_id: "UCF".to_string(), // References unknown sample
                assay: None,
                uri: None,
                path: None,
                url: None,
            }],
        };

        let err = build_genome_canvas_payload(request).unwrap_err();
        match err {
            CanvasError::InvalidGenomeCanvasPayload(issues) => {
                let issue = issues
                    .iter()
                    .find(|i| i.path == "tracks[0].sample_id")
                    .unwrap();
                assert_eq!(issue.code, "unknown_reference");
                assert!(
                    issue.message.contains("UCF"),
                    "error message should mention the unknown sample_id"
                );
            }
            _ => panic!("expected InvalidGenomeCanvasPayload error"),
        }
    }

    #[test]
    fn genome_canvas_builder_normalizes_atac_assay_and_signal_source() {
        let request = GenomeCanvasOpenRequest {
            assembly: "hg38".to_string(),
            region: "chr11:47354860-47395607".to_string(),
            gene_symbol: Some("SPI1".to_string()),
            samples: vec![GenomeCanvasSampleInput {
                id: "E3-1_ATAC12".to_string(),
                label: Some("E3-1_ATAC12".to_string()),
                assay: Some("ATAC-seq".to_string()),
                uri: Some("gewu://sample/E3-1_ATAC12".to_string()),
            }],
            tracks: vec![GenomeCanvasTrackInput {
                id: "signal-atac-E3-1_ATAC12".to_string(),
                track_type: TrackType::Signal,
                source: Some("signal".to_string()),
                sample_id: "E3-1_ATAC12".to_string(),
                assay: Some("ATAC-seq".to_string()),
                uri: Some("gewu://track/E3-1_ATAC12_signal".to_string()),
                path: None,
                url: None,
            }],
        };

        let payload = build_genome_canvas_payload(request).unwrap();
        assert_eq!(payload["samples"][0]["assay"], "atac");
        assert_eq!(payload["tracks"][0]["assay"], "atac");
        assert_eq!(payload["tracks"][0]["source"], "coverage");
    }

    #[test]
    fn genome_canvas_builder_rejects_unsupported_signal_source() {
        let request = GenomeCanvasOpenRequest {
            assembly: "hg38".to_string(),
            region: "chr11:47354860-47395607".to_string(),
            gene_symbol: None,
            samples: vec![GenomeCanvasSampleInput {
                id: "E3-1_ATAC12".to_string(),
                label: None,
                assay: Some("atac".to_string()),
                uri: None,
            }],
            tracks: vec![GenomeCanvasTrackInput {
                id: "bad".to_string(),
                track_type: TrackType::Signal,
                source: Some("bogus".to_string()),
                sample_id: "E3-1_ATAC12".to_string(),
                assay: Some("atac".to_string()),
                uri: None,
                path: None,
                url: None,
            }],
        };

        let err = build_genome_canvas_payload(request).unwrap_err();
        match err {
            CanvasError::InvalidGenomeCanvasPayload(issues) => {
                let issue = issues
                    .iter()
                    .find(|issue| issue.path == "tracks[0].source")
                    .unwrap();
                assert_eq!(issue.code, "source_invalid");
                assert!(issue.message.contains("coverage"));
            }
            _ => panic!("expected InvalidGenomeCanvasPayload error"),
        }
    }

    #[tokio::test]
    async fn open_rejects_raw_payload_with_noncanonical_signal_source() {
        let m = CanvasManager::new();
        let err = m
            .open(
                "s1",
                None,
                CanvasKind::Genome,
                CanvasMode::Overlay,
                None,
                Some(serde_json::json!({
                    "assembly": "hg38",
                    "region": "chr11:47354860-47395607",
                    "samples": [{"id": "E3-1_ATAC12"}],
                    "tracks": [{"id": "bad", "type": "signal", "sample_id": "E3-1_ATAC12", "source": "signal"}]
                })),
            )
            .await
            .unwrap_err();

        match err {
            CanvasError::InvalidGenomeCanvasPayload(issues) => {
                let issue = issues
                    .iter()
                    .find(|issue| issue.path == "tracks[0].source")
                    .unwrap();
                assert_eq!(issue.code, "source_invalid");
                assert!(issue.message.contains("coverage"));
            }
            _ => panic!("expected InvalidGenomeCanvasPayload error"),
        }
    }
}
