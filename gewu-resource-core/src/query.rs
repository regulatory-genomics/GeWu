use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Resource, ResourceStatus};
use crate::uri::ResourceUri;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl SearchRequest {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            kind: None,
            labels: BTreeMap::new(),
            limit: default_limit(),
        }
    }

    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

fn default_limit() -> usize {
    20
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ResourceCandidate {
    pub uri: String,
    pub kind: String,
    pub name: String,
    pub score: f64,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub matched: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<Resource>,
    pub status: ResourceStatus,
}

impl ResourceCandidate {
    pub fn from_resource(
        resource: &Resource,
        score: f64,
        matched: Vec<String>,
        include: bool,
    ) -> Self {
        Self {
            uri: resource.uri.to_string(),
            kind: resource.kind.clone(),
            name: resource.name.clone(),
            score,
            labels: resource.labels.clone(),
            matched,
            resource: include.then(|| resource.clone()),
            status: resource.status.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchResponse {
    pub candidates: Vec<ResourceCandidate>,
    pub total: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResolveRequest {
    pub query: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl ResolveRequest {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            kind: None,
            labels: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResolveStatus {
    Ok,
    Ambiguous,
    NotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResolveResponse {
    pub status: ResolveStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<ResourceCandidate>,
    #[serde(default)]
    pub candidates: Vec<ResourceCandidate>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DescribeRequest {
    pub uri: ResourceUri,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ValidateRequest {
    pub value: Value,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ValidateResponse {
    pub ok: bool,
    pub message: String,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_fix: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: Value,
}
