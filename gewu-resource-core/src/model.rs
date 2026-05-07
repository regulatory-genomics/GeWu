use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::uri::ResourceUri;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResourceStatus {
    Available,
    Missing,
    #[default]
    Unchecked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct ResourceProvenance {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub loaded_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Resource {
    pub uri: ResourceUri,
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    #[serde(default)]
    pub spec: Value,
    #[serde(default)]
    pub status: ResourceStatus,
    #[serde(default)]
    pub provenance: Option<ResourceProvenance>,
}

impl Resource {
    pub fn new(uri: ResourceUri, name: impl Into<String>) -> Self {
        Self {
            kind: uri.kind().to_string(),
            uri,
            name: name.into(),
            aliases: Vec::new(),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            spec: Value::Object(Default::default()),
            status: ResourceStatus::Unchecked,
            provenance: None,
        }
    }

    pub fn with_aliases<I, S>(mut self, aliases: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.aliases = aliases.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    pub fn with_annotation(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.annotations.insert(key.into(), value.into());
        self
    }

    pub fn with_spec(mut self, spec: Value) -> Self {
        self.spec = spec;
        self
    }

    pub fn with_status(mut self, status: ResourceStatus) -> Self {
        self.status = status;
        self
    }

    pub fn searchable_text(&self) -> String {
        let mut parts = vec![self.uri.to_string(), self.kind.clone(), self.name.clone()];
        parts.extend(self.aliases.clone());
        parts.extend(
            self.labels
                .iter()
                .flat_map(|(key, value)| [key.clone(), value.clone()]),
        );
        parts.extend(
            self.annotations
                .iter()
                .flat_map(|(key, value)| [key.clone(), value.clone()]),
        );
        parts.push(self.spec.to_string());
        parts.join(" ")
    }

    pub fn label_matches(&self, filters: &[(String, String)]) -> bool {
        filters
            .iter()
            .all(|(key, value)| self.labels.get(key) == Some(value))
    }
}
