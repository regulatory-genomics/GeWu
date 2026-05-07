use async_trait::async_trait;

use crate::error::ResourceResult;
use crate::model::Resource;
use crate::query::{DescribeRequest, SearchRequest, ValidateRequest, ValidateResponse};
use crate::uri::ResourceUri;

#[async_trait]
pub trait ResourceProvider: Send + Sync {
    fn id(&self) -> &str;
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
            return Ok(ValidateResponse {
                ok: false,
                message: "value is not a valid resource uri".into(),
                warnings: Vec::new(),
                code: None,
                field: None,
                retryable: None,
                suggested_fix: None,
                details: serde_json::Value::Null,
            });
        };
        if request.kind.as_ref().is_some_and(|kind| uri.kind() != kind) {
            return Ok(ValidateResponse {
                ok: false,
                message: "resource kind does not match request".into(),
                warnings: Vec::new(),
                code: None,
                field: None,
                retryable: None,
                suggested_fix: None,
                details: serde_json::Value::Null,
            });
        }
        let exists = self.describe(&DescribeRequest { uri }).await?.is_some();
        Ok(ValidateResponse {
            ok: exists,
            message: if exists {
                "resource found"
            } else {
                "resource not found"
            }
            .into(),
            warnings: Vec::new(),
            code: None,
            field: None,
            retryable: None,
            suggested_fix: None,
            details: serde_json::Value::Null,
        })
    }
}
