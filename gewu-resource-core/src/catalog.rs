use std::collections::BTreeMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ResourceError, ResourceResult};
use crate::model::{Resource, ResourceProvenance, ResourceStatus};
use crate::uri::ResourceUri;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResourceCatalogEntry {
    pub uri: ResourceUri,
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

impl ResourceCatalogEntry {
    pub fn from_resource(resource: Resource) -> Self {
        Self {
            uri: resource.uri,
            name: resource.name,
            aliases: resource.aliases,
            labels: resource.labels,
            annotations: resource.annotations,
            spec: resource.spec,
            status: resource.status,
            provenance: resource.provenance,
        }
    }

    fn into_resource(self) -> Resource {
        Resource {
            kind: self.uri.kind().to_string(),
            uri: self.uri,
            name: self.name,
            aliases: self.aliases,
            labels: self.labels,
            annotations: self.annotations,
            spec: self.spec,
            status: self.status,
            provenance: self.provenance,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct ResourceCatalog {
    #[serde(default)]
    pub resources: Vec<ResourceCatalogEntry>,
}

impl ResourceCatalog {
    pub fn into_resources(self) -> ResourceResult<Vec<Resource>> {
        Ok(self
            .resources
            .into_iter()
            .map(ResourceCatalogEntry::into_resource)
            .collect())
    }
}

pub struct CatalogLoader;

impl CatalogLoader {
    pub fn load_file(path: impl AsRef<Path>) -> ResourceResult<Vec<Resource>> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .map_err(|err| ResourceError::Catalog(format!("{}: {err}", path.display())))?;
        let catalog: ResourceCatalog = serde_yaml::from_str(&content)
            .map_err(|err| ResourceError::Catalog(format!("{}: {err}", path.display())))?;
        catalog.into_resources()
    }

    pub fn load_dir(path: impl AsRef<Path>) -> ResourceResult<Vec<Resource>> {
        let mut files = std::fs::read_dir(path.as_ref())
            .map_err(|err| ResourceError::Catalog(err.to_string()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| ResourceError::Catalog(err.to_string()))?;
        files.retain(|path| {
            matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("yaml" | "yml")
            )
        });
        files.sort();

        let mut resources = Vec::new();
        for file in files {
            resources.extend(Self::load_file(file)?);
        }
        Ok(resources)
    }
}
