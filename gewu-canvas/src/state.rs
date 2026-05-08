//! Rust mirror of `tachyon-canvas/contracts/canvas-state.ts` and
//! `tachyon-canvas/contracts/genome-canvas-state.ts`.
//!
//! Source of truth is the TypeScript contract; field renames here MUST stay
//! in lockstep. Bump [`CANVAS_STATE_VERSION`] on any breaking change.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const CANVAS_STATE_VERSION: u32 = 1;

/// Top-level canvas surface kind. Drives which frontend component renders the
/// payload; GeWu does not interpret the payload itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CanvasKind {
    Genome,
    Hic,
    Umap,
    Table,
    Report,
    Custom,
}

impl CanvasKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Genome => "genome",
            Self::Hic => "hic",
            Self::Umap => "umap",
            Self::Table => "table",
            Self::Report => "report",
            Self::Custom => "custom",
        }
    }
}

/// How the canvas is laid out in the host UI.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum CanvasMode {
    Overlay,
    #[default]
    Docked,
    Fullscreen,
}

/// The full state for a single canvas instance.
///
/// `payload` is `serde_json::Value` because GeWu does not dispatch on kind:
/// it persists arbitrary JSON. The kind-specific renderer (e.g. tachyon-canvas
/// `GenomeCanvas`) is responsible for parsing it as [`GenomeCanvasState`] etc.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CanvasState {
    pub canvas_id: String,
    pub kind: CanvasKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub mode: CanvasMode,
    #[serde(default = "default_true")]
    pub visible: bool,
    #[serde(default)]
    pub focused: bool,
    #[serde(default = "serde_json::Value::default")]
    pub payload: serde_json::Value,
    /// ISO-8601 string. Updated on every successful merge.
    pub updated_at: String,
}

fn default_true() -> bool {
    true
}

/// Patch shape for `canvas.set_state`.
///
/// Top-level fields replace; `payload` shallow-merges (one level deep). To
/// clear a payload key, set it to `null`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CanvasStatePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CanvasMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Map<String, serde_json::Value>>,
}

impl CanvasState {
    pub fn new(canvas_id: impl Into<String>, kind: CanvasKind) -> Self {
        Self {
            canvas_id: canvas_id.into(),
            kind,
            title: None,
            mode: CanvasMode::Docked,
            visible: true,
            focused: true,
            payload: serde_json::Value::Object(Default::default()),
            updated_at: now_iso(),
        }
    }

    /// Apply `patch` in place. Returns `true` if anything changed.
    ///
    /// Top-level scalar fields are replaced when `Some`. `payload` is
    /// shallow-merged: each key in the patch's payload object replaces the
    /// corresponding key in the existing payload (use `null` to clear). Nested
    /// objects are NOT recursively merged — that's the caller's responsibility
    /// when they want field-level merges inside the payload (e.g. updating
    /// `region` without touching `samples` is naturally one-level).
    pub fn apply_patch(&mut self, patch: CanvasStatePatch) -> bool {
        let mut changed = false;
        if let Some(title) = patch.title {
            self.title = if title.is_empty() { None } else { Some(title) };
            changed = true;
        }
        if let Some(mode) = patch.mode {
            self.mode = mode;
            changed = true;
        }
        if let Some(visible) = patch.visible {
            self.visible = visible;
            changed = true;
        }
        if let Some(focused) = patch.focused {
            self.focused = focused;
            changed = true;
        }
        if let Some(payload_patch) = patch.payload {
            let target = match &mut self.payload {
                serde_json::Value::Object(map) => map,
                other => {
                    *other = serde_json::Value::Object(Default::default());
                    other.as_object_mut().expect("just inserted object")
                }
            };
            for (key, value) in payload_patch {
                target.insert(key, value);
            }
            changed = true;
        }
        if changed {
            self.updated_at = now_iso();
        }
        changed
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// ---------------------------------------------------------------------------
// Genome-specific payload (mirrors genome-canvas-state.ts)
// ---------------------------------------------------------------------------

/// Genome-canvas payload. Used as the `payload` of [`CanvasState`] when
/// `kind = Genome`.
///
/// Lives in this crate so server-side tools and tests can build typed
/// instances. The actual rendering happens in tachyon-canvas.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GenomeCanvasState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assembly: Option<String>,
    /// Region as `chr:start-end` (UCSC-style, 1-based inclusive).
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub samples: Vec<SampleSpec>,
    #[serde(default)]
    pub tracks: Vec<TrackSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<GenomeSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<GenomeViewport>,
    /// Last-attached evidence bundle. Kept as `Value` so this crate doesn't
    /// have to depend on the `RegionEvidenceReport` shape — that schema is
    /// owned by tachyon-canvas/tachyon-browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<serde_json::Value>,
    /// Forward-compat: any unknown keys are preserved instead of dropped, so
    /// older GeWu builds don't lose data added by newer canvas frontends.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SampleSpec {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assay: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicate_group: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackSpec {
    pub id: String,
    #[serde(rename = "type")]
    pub track_type: TrackType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assay: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default = "default_true")]
    pub visible: bool,
}

impl Default for TrackSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            track_type: TrackType::default(),
            source: None,
            sample_id: None,
            assay: None,
            color: None,
            // Match the serde default so Rust-side construction and
            // deserialized JSON agree.
            visible: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum TrackType {
    #[default]
    Signal,
    Peak,
    Gene,
    Annotation,
    Qc,
}

// ---------------------------------------------------------------------------
// Genome canvas payload builder input types
// ---------------------------------------------------------------------------

/// Input for building a canonical genome canvas payload.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GenomeCanvasOpenRequest {
    pub assembly: String,
    pub region: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gene_symbol: Option<String>,
    #[serde(default)]
    pub samples: Vec<GenomeCanvasSampleInput>,
    #[serde(default)]
    pub tracks: Vec<GenomeCanvasTrackInput>,
}

/// Input for a sample within [`GenomeCanvasOpenRequest`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GenomeCanvasSampleInput {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assay: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

/// Input for a track within [`GenomeCanvasOpenRequest`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenomeCanvasTrackInput {
    pub id: String,
    #[serde(rename = "type")]
    pub track_type: TrackType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub sample_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assay: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Build a canonical genome canvas payload from an open request.
pub fn build_genome_canvas_payload(
    request: GenomeCanvasOpenRequest,
) -> Result<serde_json::Value, crate::manager::CanvasError> {
    use crate::manager::{CanvasError, CanvasValidationIssue};

    let mut issues = Vec::new();

    // Validate required fields
    if request.assembly.trim().is_empty() {
        issues.push(CanvasValidationIssue {
            path: "assembly".to_string(),
            code: "missing_required",
            message: "genome canvas payload requires non-empty assembly".to_string(),
        });
    }

    if request.region.trim().is_empty() {
        issues.push(CanvasValidationIssue {
            path: "region".to_string(),
            code: "missing_required",
            message: "genome canvas payload requires non-empty region".to_string(),
        });
    }

    // Validate and collect sample ids
    let mut sample_ids = std::collections::HashSet::new();
    for (index, sample) in request.samples.iter().enumerate() {
        if sample.id.trim().is_empty() {
            issues.push(CanvasValidationIssue {
                path: format!("samples[{index}].id"),
                code: "missing_required",
                message: "sample requires non-empty id".to_string(),
            });
        } else {
            sample_ids.insert(sample.id.clone());
        }
    }

    // Validate track ids and sample references
    for (index, track) in request.tracks.iter().enumerate() {
        if track.id.trim().is_empty() {
            issues.push(CanvasValidationIssue {
                path: format!("tracks[{index}].id"),
                code: "missing_required",
                message: "track requires non-empty id".to_string(),
            });
        }

        if track.sample_id.trim().is_empty() {
            issues.push(CanvasValidationIssue {
                path: format!("tracks[{index}].sample_id"),
                code: "unknown_reference",
                message: "track requires non-empty sample_id".to_string(),
            });
        } else if !sample_ids.contains(&track.sample_id) {
            issues.push(CanvasValidationIssue {
                path: format!("tracks[{index}].sample_id"),
                code: "unknown_reference",
                message: format!("track references unknown sample_id '{}'", track.sample_id),
            });
        }
    }

    if !issues.is_empty() {
        return Err(CanvasError::InvalidGenomeCanvasPayload(issues));
    }

    // Build canonical JSON payload
    let samples: Vec<serde_json::Value> = request
        .samples
        .iter()
        .map(|s| {
            let mut obj = serde_json::json!({ "id": s.id });
            if let Some(label) = &s.label {
                obj["label"] = serde_json::json!(label);
            }
            if let Some(assay) = &s.assay {
                obj["assay"] = serde_json::json!(assay);
            }
            if let Some(uri) = &s.uri {
                obj["uri"] = serde_json::json!(uri);
            }
            obj
        })
        .collect();

    let tracks: Vec<serde_json::Value> = request
        .tracks
        .iter()
        .map(|t| {
            let mut obj = serde_json::json!({
                "id": t.id,
                "type": t.track_type,
                "sample_id": t.sample_id,
                "visible": true,
            });
            if let Some(source) = &t.source {
                obj["source"] = serde_json::json!(source);
            }
            if let Some(assay) = &t.assay {
                obj["assay"] = serde_json::json!(assay);
            }
            if let Some(uri) = &t.uri {
                obj["uri"] = serde_json::json!(uri);
            }
            if let Some(path) = &t.path {
                obj["path"] = serde_json::json!(path);
            }
            if let Some(url) = &t.url {
                obj["url"] = serde_json::json!(url);
            }
            obj
        })
        .collect();

    let mut payload = serde_json::json!({
        "assembly": request.assembly,
        "region": request.region,
        "samples": samples,
        "tracks": tracks,
    });

    if let Some(gene_symbol) = &request.gene_symbol {
        if !gene_symbol.trim().is_empty() {
            payload["gene"] = serde_json::json!({
                "symbol": gene_symbol,
                "region": request.region,
            });
        }
    }

    Ok(payload)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GenomeSelection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GenomeViewport {
    pub width: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bins: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom_level: Option<f32>,
}

// ---------------------------------------------------------------------------
// SSE event envelopes (piggyback on tool events; see sse-bridge.md)
// ---------------------------------------------------------------------------

/// Discriminated envelope for the `event` field rides on
/// `tool_start.metadata` (open) or `tool_finish.output` (patch/close/focus).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum CanvasEvent {
    #[serde(rename = "canvas:open")]
    Open {
        canvas_id: String,
        kind: CanvasKind,
        mode: CanvasMode,
        initial_state: CanvasState,
    },
    #[serde(rename = "canvas:patch")]
    Patch {
        canvas_id: String,
        patch: CanvasStatePatch,
        updated_at: String,
    },
    #[serde(rename = "canvas:close")]
    Close { canvas_id: String },
    #[serde(rename = "canvas:focus")]
    Focus { canvas_id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_canvas_defaults_to_docked_mode() {
        let state = CanvasState::new("c-1", CanvasKind::Genome);
        assert_eq!(state.mode, CanvasMode::Docked);
    }

    #[test]
    fn apply_patch_merges_payload_shallow() {
        let mut state = CanvasState::new("c-1", CanvasKind::Genome);
        state.payload = serde_json::json!({ "region": "chr1:1-100", "samples": ["a"] });

        let patch = serde_json::from_value::<CanvasStatePatch>(serde_json::json!({
            "payload": { "region": "chr2:1-100" }
        }))
        .unwrap();
        assert!(state.apply_patch(patch));

        assert_eq!(state.payload["region"], "chr2:1-100");
        // samples must survive the merge
        assert_eq!(state.payload["samples"][0], "a");
    }

    #[test]
    fn apply_patch_clear_payload_key_with_null() {
        let mut state = CanvasState::new("c-1", CanvasKind::Genome);
        state.payload = serde_json::json!({ "region": "chr1:1-100" });

        let patch = serde_json::from_value::<CanvasStatePatch>(serde_json::json!({
            "payload": { "region": null }
        }))
        .unwrap();
        state.apply_patch(patch);
        assert!(state.payload["region"].is_null());
    }

    #[test]
    fn canvas_event_serializes_with_tagged_event_field() {
        let event = CanvasEvent::Open {
            canvas_id: "c-1".into(),
            kind: CanvasKind::Genome,
            mode: CanvasMode::Overlay,
            initial_state: CanvasState::new("c-1", CanvasKind::Genome),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["event"], "canvas:open");
        assert_eq!(json["canvas_id"], "c-1");
        assert_eq!(json["kind"], "genome");
    }

    #[test]
    fn track_type_serializes_lowercase() {
        let track = TrackSpec {
            id: "t1".into(),
            track_type: TrackType::Signal,
            ..Default::default()
        };
        let json = serde_json::to_value(&track).unwrap();
        assert_eq!(json["type"], "signal");
        assert_eq!(json["visible"], true);
    }

    #[test]
    fn genome_state_extra_fields_survive_roundtrip() {
        let raw = serde_json::json!({
            "region": "chr1:1-100",
            "samples": [],
            "tracks": [],
            "future_field_we_dont_know_about": { "x": 1 }
        });
        let parsed: GenomeCanvasState = serde_json::from_value(raw).unwrap();
        let back = serde_json::to_value(&parsed).unwrap();
        assert_eq!(back["future_field_we_dont_know_about"]["x"], 1);
    }
}
