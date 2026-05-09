use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::error::ResourceResult;
use crate::model::Resource;
use crate::query::{DescribeRequest, SearchRequest, ValidateRequest, ValidateResponse};
use crate::uri::ResourceUri;

#[async_trait]
pub trait ResourceProvider: Send + Sync {
    fn id(&self) -> &str;
    fn is_mutable(&self) -> bool {
        false
    }
    fn kinds(&self) -> Vec<String>;
    async fn search(&self, request: &SearchRequest) -> ResourceResult<Vec<Resource>>;
    async fn describe(&self, request: &DescribeRequest) -> ResourceResult<Option<Resource>>;
    async fn validate(&self, request: &ValidateRequest) -> ResourceResult<ValidateResponse>;
}

#[derive(Debug, Clone)]
pub struct InMemoryResourceProvider {
    id: String,
    resources: Vec<Resource>,
}

impl InMemoryResourceProvider {
    pub fn new(id: impl Into<String>, resources: Vec<Resource>) -> Self {
        Self {
            id: id.into(),
            resources,
        }
    }

    fn label_filters(request: &SearchRequest) -> Vec<(String, String)> {
        request
            .labels
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    fn matches_search(resource: &Resource, request: &SearchRequest) -> bool {
        request
            .kind
            .as_ref()
            .is_none_or(|kind| &resource.kind == kind)
            && resource.label_matches(&Self::label_filters(request))
            && resource
                .searchable_text()
                .to_lowercase()
                .contains(&request.query.to_lowercase())
    }
}

#[async_trait]
impl ResourceProvider for InMemoryResourceProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn kinds(&self) -> Vec<String> {
        let mut kinds = self
            .resources
            .iter()
            .map(|resource| resource.kind.clone())
            .collect::<Vec<_>>();
        kinds.sort();
        kinds.dedup();
        kinds
    }

    async fn search(&self, request: &SearchRequest) -> ResourceResult<Vec<Resource>> {
        Ok(self
            .resources
            .iter()
            .filter(|resource| Self::matches_search(resource, request))
            .cloned()
            .collect())
    }

    async fn describe(&self, request: &DescribeRequest) -> ResourceResult<Option<Resource>> {
        Ok(self
            .resources
            .iter()
            .find(|resource| resource.uri == request.uri)
            .cloned())
    }

    async fn validate(&self, request: &ValidateRequest) -> ResourceResult<ValidateResponse> {
        let uri: Option<ResourceUri> = request.value.as_str().and_then(|value| value.parse().ok());
        let Some(uri) = uri else {
            let suggested_uri = generate_uri_suggestion(&request.value, request.kind.as_deref());
            return Ok(ValidateResponse {
                ok: false,
                message: "value is not a valid resource uri".into(),
                warnings: Vec::new(),
                code: Some("invalid_resource_uri".into()),
                field: Some("value".into()),
                retryable: Some(false),
                suggested_fix: Some(format!(
                    "Pass a canonical resource URI string such as {suggested_uri}, not an object."
                )),
                details: serde_json::json!({
                    "expected_format": "gewu://<kind>/<id>",
                    "example": "gewu://sample/ucf_atac_hg38"
                }),
            });
        };
        if request.kind.as_ref().is_some_and(|kind| uri.kind() != kind) {
            return Ok(ValidateResponse {
                ok: false,
                message: "resource kind does not match request".into(),
                warnings: Vec::new(),
                code: Some("resource_kind_mismatch".into()),
                field: Some("kind".into()),
                retryable: Some(false),
                suggested_fix: Some(
                    request
                        .kind
                        .as_ref()
                        .map(|kind| {
                            format!(
                                "Use a URI whose kind matches the requested kind, for example gewu://{kind}/<id> for kind={kind}."
                            )
                        })
                        .unwrap_or_else(|| {
                            "Use a URI whose kind matches the requested kind.".into()
                        }),
                ),
                details: serde_json::json!({
                    "uri_kind": uri.kind(),
                    "requested_kind": request.kind
                }),
            });
        }
        let exists = self.describe(&DescribeRequest { uri }).await?.is_some();
        if !exists {
            return Ok(ValidateResponse {
                ok: false,
                message: "resource not found".into(),
                warnings: Vec::new(),
                code: Some("resource_not_found".into()),
                field: Some("value".into()),
                retryable: Some(false),
                suggested_fix: Some(
                    "Register this resource with 'resource.register' or search with 'resource.search' to find similar resources".into()
                ),
                details: serde_json::Value::Null,
            });
        }
        Ok(ValidateResponse {
            ok: true,
            message: "resource found".into(),
            warnings: Vec::new(),
            code: None,
            field: None,
            retryable: None,
            suggested_fix: None,
            details: serde_json::Value::Null,
        })
    }
}

fn generate_uri_suggestion(value: &serde_json::Value, requested_kind: Option<&str>) -> String {
    let obj = match value.as_object() {
        Some(obj) => obj,
        None => return "gewu://sample/unnamed".to_string(),
    };

    let kind = requested_kind
        .map(String::from)
        .or_else(|| {
            if obj.contains_key("assay") {
                obj.get("assay")
                    .and_then(|v| v.as_str())
                    .map(|_| "sample".to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "sample".to_string());

    let id = obj
        .get("sample")
        .and_then(|value| value.as_str())
        .or_else(|| obj.get("sample_id").and_then(|value| value.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            obj.get("path")
                .and_then(|value| value.as_str())
                .and_then(canonical_sample_id_from_ucf_path)
        })
        .unwrap_or_else(|| "unnamed".to_string());

    format!("gewu://{kind}/{id}")
}

fn canonical_sample_id_from_ucf_path(path: &str) -> Option<String> {
    let file_name = std::path::Path::new(path).file_name()?.to_str()?.trim();
    if file_name.is_empty() {
        return None;
    }

    file_name
        .strip_suffix(".filtered.ucf")
        .or_else(|| file_name.strip_suffix(".ucf"))
        .map(ToOwned::to_owned)
}

#[derive(Debug)]
pub struct MutableResourceProvider {
    id: String,
    resources: RwLock<Vec<Resource>>,
}

impl MutableResourceProvider {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            resources: RwLock::new(Vec::new()),
        }
    }

    pub fn contains_uri(&self, uri: &ResourceUri) -> ResourceResult<bool> {
        let resources = self
            .resources
            .read()
            .map_err(|err| crate::error::ResourceError::Provider(err.to_string()))?;
        Ok(resources.iter().any(|resource| &resource.uri == uri))
    }

    pub fn upsert(&self, resource: Resource) -> ResourceResult<()> {
        self.upsert_many(vec![resource])
    }

    pub fn upsert_many(&self, resources_to_upsert: Vec<Resource>) -> ResourceResult<()> {
        let mut resources = self
            .resources
            .write()
            .map_err(|err| crate::error::ResourceError::Provider(err.to_string()))?;
        for resource in resources_to_upsert {
            if let Some(existing) = resources
                .iter_mut()
                .find(|existing| existing.uri == resource.uri)
            {
                *existing = resource;
            } else {
                resources.push(resource);
            }
        }
        Ok(())
    }

    fn snapshot(&self) -> ResourceResult<Vec<Resource>> {
        let resources = self
            .resources
            .read()
            .map_err(|err| crate::error::ResourceError::Provider(err.to_string()))?;
        Ok(resources.clone())
    }
}

#[async_trait]
impl ResourceProvider for MutableResourceProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn is_mutable(&self) -> bool {
        true
    }

    fn kinds(&self) -> Vec<String> {
        let Ok(resources) = self.resources.read() else {
            return Vec::new();
        };
        let mut kinds = resources
            .iter()
            .map(|resource| resource.kind.clone())
            .collect::<Vec<_>>();
        kinds.sort();
        kinds.dedup();
        kinds
    }

    async fn search(&self, request: &SearchRequest) -> ResourceResult<Vec<Resource>> {
        Ok(self
            .snapshot()?
            .into_iter()
            .filter(|resource| InMemoryResourceProvider::matches_search(resource, request))
            .collect())
    }

    async fn describe(&self, request: &DescribeRequest) -> ResourceResult<Option<Resource>> {
        Ok(self
            .snapshot()?
            .into_iter()
            .find(|resource| resource.uri == request.uri))
    }

    async fn validate(&self, request: &ValidateRequest) -> ResourceResult<ValidateResponse> {
        let provider = InMemoryResourceProvider::new(self.id.clone(), self.snapshot()?);
        provider.validate(request).await
    }
}

#[async_trait]
impl ResourceProvider for Arc<MutableResourceProvider> {
    fn id(&self) -> &str {
        self.as_ref().id()
    }

    fn is_mutable(&self) -> bool {
        true
    }

    fn kinds(&self) -> Vec<String> {
        self.as_ref().kinds()
    }

    async fn search(&self, request: &SearchRequest) -> ResourceResult<Vec<Resource>> {
        self.as_ref().search(request).await
    }

    async fn describe(&self, request: &DescribeRequest) -> ResourceResult<Option<Resource>> {
        self.as_ref().describe(request).await
    }

    async fn validate(&self, request: &ValidateRequest) -> ResourceResult<ValidateResponse> {
        self.as_ref().validate(request).await
    }
}
