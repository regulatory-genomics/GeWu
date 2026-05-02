//! Genome-analysis tools that POST to tachyon-browser's
//! `/api/tools/genome/*` endpoints.
//!
//! These are the agent-callable counterparts to the canvas.* tools: where
//! `canvas.*` controls the frontend surface, `genome.*` runs domain analyses
//! and returns a `RegionEvidenceReport`. The agent typically calls them in
//! sequence:
//!
//!   1. `canvas.get_state` → see what the user is viewing.
//!   2. `genome.inspect_region` (or compare_samples / local_qc) → analyse it.
//!   3. `canvas.set_state` → attach the evidence bundle to the canvas.
//!
//! Each tool here is a thin HTTPS client. The structured output flows
//! through to the agent verbatim, plus the same JSON travels through
//! `tool_finish.output` for the frontend trace UI.

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolExecutionPolicy, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct GenomeToolsConfig {
    /// Base URL of a running tachyon-browser, e.g. "http://127.0.0.1:8400".
    /// Empty string disables the genome tools (they will still register but
    /// return a clear error).
    pub base_url: String,
    /// Per-request timeout. Defaults to 60s.
    pub timeout: Duration,
}

impl Default for GenomeToolsConfig {
    fn default() -> Self {
        Self {
            base_url: std::env::var("TACHYON_BROWSER_URL").unwrap_or_default(),
            timeout: Duration::from_secs(60),
        }
    }
}

/// Build the five genome tools, sharing an HTTP client + base URL config.
///
/// Returns an empty vec when `base_url` is empty so callers can drop them
/// silently if tachyon-browser isn't configured.
pub fn genome_tools(config: GenomeToolsConfig) -> Vec<Arc<dyn Tool>> {
    if config.base_url.trim().is_empty() {
        tracing::warn!(
            "GenomeToolsConfig.base_url is empty; genome.* tools will not be registered"
        );
        return Vec::new();
    }
    let client = match reqwest::Client::builder().timeout(config.timeout).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to build reqwest client for genome tools: {e}");
            return Vec::new();
        }
    };
    let shared = Arc::new(SharedClient {
        client,
        base_url: config.base_url.trim_end_matches('/').to_string(),
    });
    vec![
        Arc::new(GenomeInspectRegionTool { shared: shared.clone() }),
        Arc::new(GenomeCompareSamplesTool { shared: shared.clone() }),
        Arc::new(GenomeLocalQcTool { shared: shared.clone() }),
        Arc::new(GenomeScanCandidatesTool { shared: shared.clone() }),
        Arc::new(GenomeExplainRegionTool { shared }),
    ]
}

struct SharedClient {
    client: reqwest::Client,
    base_url: String,
}

impl SharedClient {
    async fn post_json(&self, path: &str, body: &Value) -> Result<Value, GenomeToolError> {
        let url = format!("{}{path}", self.base_url);
        let resp = self.client.post(&url).json(body).send().await.map_err(|e| {
            GenomeToolError::Transport(format!("POST {url} failed: {e}"))
        })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(GenomeToolError::HttpStatus { status, body: text });
        }
        serde_json::from_str::<Value>(&text)
            .map_err(|e| GenomeToolError::BadJson(format!("{e}: {text}")))
    }
}

#[derive(Debug)]
enum GenomeToolError {
    Transport(String),
    HttpStatus { status: reqwest::StatusCode, body: String },
    BadJson(String),
}

impl GenomeToolError {
    fn into_tool_output(self, tool: &str) -> ToolResult {
        let payload = match self {
            GenomeToolError::Transport(msg) => json!({
                "error": { "code": "transport_error", "message": msg }
            }),
            GenomeToolError::HttpStatus { status, body } => json!({
                "error": {
                    "code": "http_error",
                    "status": status.as_u16(),
                    "message": body,
                }
            }),
            GenomeToolError::BadJson(msg) => json!({
                "error": { "code": "bad_response", "message": msg }
            }),
        };
        ToolResult::new(tool, payload.to_string())
    }
}

fn schema_from(v: Value) -> RootSchema {
    serde_json::from_value(v).expect("static schema must parse")
}

fn box_err<E: Into<Box<dyn Error + Send + Sync>>>(e: E) -> Box<dyn Error + Send + Sync> {
    e.into()
}

// ---------------------------------------------------------------------------
// Shared sample/region payload
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct SampleRefArg {
    pub sample: String,
    pub assay: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

fn sample_array_schema() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "sample": { "type": "string" },
                "assay": { "type": "string" },
                "source": { "type": "string", "description": "Signal source (e.g. coverage, l1)." }
            },
            "required": ["sample", "assay"]
        }
    })
}

// ---------------------------------------------------------------------------
// genome.inspect_region
// ---------------------------------------------------------------------------

pub struct GenomeInspectRegionTool {
    shared: Arc<SharedClient>,
}

#[derive(Deserialize, Serialize)]
struct InspectArgs {
    region: String,
    #[serde(default)]
    samples: Vec<SampleRefArg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bins: Option<u32>,
}

#[async_trait]
impl Tool for GenomeInspectRegionTool {
    fn name(&self) -> &str { "genome.inspect_region" }
    fn description(&self) -> &str {
        "Run a region-level inspection on the given samples and return a RegionEvidenceReport \
         (observations, technical_qc, evidence_items, suggested_next_steps). Use AFTER \
         canvas.get_state so you query the region the user is viewing."
    }
    fn schema(&self) -> RootSchema {
        schema_from(json!({
            "type": "object",
            "properties": {
                "region": { "type": "string", "description": "chr:start-end" },
                "samples": sample_array_schema(),
                "bins": { "type": "integer", "minimum": 16, "maximum": 8192 }
            },
            "required": ["region"]
        }))
    }
    fn execution_policy(&self) -> ToolExecutionPolicy { ToolExecutionPolicy::ParallelSafe }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let parsed: InspectArgs = serde_json::from_value(args).map_err(box_err)?;
        let body = serde_json::to_value(&parsed).map_err(box_err)?;
        match self.shared.post_json("/api/tools/genome/inspect_region", &body).await {
            Ok(v) => Ok(ToolResult::new("genome.inspect_region", v.to_string())),
            Err(e) => Ok(e.into_tool_output("genome.inspect_region")),
        }
    }
}

// ---------------------------------------------------------------------------
// genome.compare_samples
// ---------------------------------------------------------------------------

pub struct GenomeCompareSamplesTool {
    shared: Arc<SharedClient>,
}

#[derive(Deserialize, Serialize)]
struct CompareArgs {
    region: String,
    sample_a: SampleRefArg,
    sample_b: SampleRefArg,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bins: Option<u32>,
}

#[async_trait]
impl Tool for GenomeCompareSamplesTool {
    fn name(&self) -> &str { "genome.compare_samples" }
    fn description(&self) -> &str {
        "Compare two samples at a region and return a RegionEvidenceReport with log2 ratio \
         and per-sample summaries. Useful for differential signal questions."
    }
    fn schema(&self) -> RootSchema {
        schema_from(json!({
            "type": "object",
            "properties": {
                "region": { "type": "string" },
                "sample_a": { "$ref": "#/definitions/sample" },
                "sample_b": { "$ref": "#/definitions/sample" },
                "bins": { "type": "integer" }
            },
            "required": ["region", "sample_a", "sample_b"],
            "definitions": {
                "sample": {
                    "type": "object",
                    "properties": {
                        "sample": { "type": "string" },
                        "assay": { "type": "string" },
                        "source": { "type": "string" }
                    },
                    "required": ["sample", "assay"]
                }
            }
        }))
    }
    fn execution_policy(&self) -> ToolExecutionPolicy { ToolExecutionPolicy::ParallelSafe }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let parsed: CompareArgs = serde_json::from_value(args).map_err(box_err)?;
        let body = serde_json::to_value(&parsed).map_err(box_err)?;
        match self.shared.post_json("/api/tools/genome/compare_samples", &body).await {
            Ok(v) => Ok(ToolResult::new("genome.compare_samples", v.to_string())),
            Err(e) => Ok(e.into_tool_output("genome.compare_samples")),
        }
    }
}

// ---------------------------------------------------------------------------
// genome.local_qc
// ---------------------------------------------------------------------------

pub struct GenomeLocalQcTool {
    shared: Arc<SharedClient>,
}

#[async_trait]
impl Tool for GenomeLocalQcTool {
    fn name(&self) -> &str { "genome.local_qc" }
    fn description(&self) -> &str {
        "Run technical QC on a region (depth, background vs flanks, replicate agreement) and \
         return a RegionEvidenceReport. Use to answer 'is this peak real or just background?'"
    }
    fn schema(&self) -> RootSchema {
        schema_from(json!({
            "type": "object",
            "properties": {
                "region": { "type": "string" },
                "samples": sample_array_schema(),
                "bins": { "type": "integer" }
            },
            "required": ["region", "samples"]
        }))
    }
    fn execution_policy(&self) -> ToolExecutionPolicy { ToolExecutionPolicy::ParallelSafe }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        match self.shared.post_json("/api/tools/genome/local_qc", &args).await {
            Ok(v) => Ok(ToolResult::new("genome.local_qc", v.to_string())),
            Err(e) => Ok(e.into_tool_output("genome.local_qc")),
        }
    }
}

// ---------------------------------------------------------------------------
// genome.scan_candidates
// ---------------------------------------------------------------------------

pub struct GenomeScanCandidatesTool {
    shared: Arc<SharedClient>,
}

#[async_trait]
impl Tool for GenomeScanCandidatesTool {
    fn name(&self) -> &str { "genome.scan_candidates" }
    fn description(&self) -> &str {
        "Scan for top candidate regions across samples by peak score. Use to suggest where \
         the user should navigate next; pair with canvas.set_state to jump there."
    }
    fn schema(&self) -> RootSchema {
        schema_from(json!({
            "type": "object",
            "properties": {
                "region": { "type": "string", "description": "chr:start-end to scan within" },
                "samples": sample_array_schema(),
                "min_score": { "type": "number", "default": 1.0 },
                "limit": { "type": "integer", "default": 25, "maximum": 200 }
            },
            "required": ["samples"]
        }))
    }
    fn execution_policy(&self) -> ToolExecutionPolicy { ToolExecutionPolicy::ParallelSafe }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        match self.shared.post_json("/api/tools/genome/scan_candidates", &args).await {
            Ok(v) => Ok(ToolResult::new("genome.scan_candidates", v.to_string())),
            Err(e) => Ok(e.into_tool_output("genome.scan_candidates")),
        }
    }
}

// ---------------------------------------------------------------------------
// genome.explain_region
// ---------------------------------------------------------------------------

pub struct GenomeExplainRegionTool {
    shared: Arc<SharedClient>,
}

#[async_trait]
impl Tool for GenomeExplainRegionTool {
    fn name(&self) -> &str { "genome.explain_region" }
    fn description(&self) -> &str {
        "Compose an interpretation of a region. Optionally accepts a pre-fetched evidence \
         bundle (from inspect_region) to avoid re-querying. Returns a RegionEvidenceReport \
         with biological_interpretation populated."
    }
    fn schema(&self) -> RootSchema {
        schema_from(json!({
            "type": "object",
            "properties": {
                "region": { "type": "string" },
                "samples": sample_array_schema(),
                "question": { "type": "string" },
                "evidence": { "type": "object" }
            },
            "required": ["region"]
        }))
    }
    fn execution_policy(&self) -> ToolExecutionPolicy { ToolExecutionPolicy::ParallelSafe }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        match self.shared.post_json("/api/tools/genome/explain_region", &args).await {
            Ok(v) => Ok(ToolResult::new("genome.explain_region", v.to_string())),
            Err(e) => Ok(e.into_tool_output("genome.explain_region")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_base_url_returns_no_tools() {
        let tools = genome_tools(GenomeToolsConfig {
            base_url: String::new(),
            timeout: Duration::from_secs(1),
        });
        assert!(tools.is_empty());
    }

    #[test]
    fn populated_base_url_returns_five_tools() {
        let tools = genome_tools(GenomeToolsConfig {
            base_url: "http://127.0.0.1:0".into(),
            timeout: Duration::from_secs(1),
        });
        let names: Vec<_> = tools.iter().map(|t| t.name().to_string()).collect();
        assert_eq!(
            names,
            vec![
                "genome.inspect_region",
                "genome.compare_samples",
                "genome.local_qc",
                "genome.scan_candidates",
                "genome.explain_region",
            ]
        );
    }
}
