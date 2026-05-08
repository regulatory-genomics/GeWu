use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::ResourceResult;
use crate::model::Resource;
use crate::provider::ResourceProvider;
use crate::query::{
    DescribeRequest, ResolveRequest, ResolveResponse, ResolveStatus, ResourceCandidate,
    SearchRequest, SearchResponse, ValidateRequest, ValidateResponse,
};

#[derive(Clone, Default)]
pub struct ResourceRegistry {
    providers: Vec<Arc<dyn ResourceProvider>>,
}

impl ResourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_provider<P>(mut self, provider: P) -> Self
    where
        P: ResourceProvider + 'static,
    {
        self.register(provider);
        self
    }

    pub fn register<P>(&mut self, provider: P)
    where
        P: ResourceProvider + 'static,
    {
        self.providers.push(Arc::new(provider));
    }

    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    pub async fn search(&self, request: SearchRequest) -> ResourceResult<SearchResponse> {
        let mut resources_by_uri = BTreeMap::new();
        for provider in &self.providers {
            for resource in provider.search(&request).await? {
                if provider.is_mutable() {
                    resources_by_uri.insert(resource.uri.clone(), resource);
                } else {
                    resources_by_uri
                        .entry(resource.uri.clone())
                        .or_insert(resource);
                }
            }
        }
        let resources = resources_by_uri.into_values().collect::<Vec<_>>();
        let total = resources.len();
        let limit = request.limit.max(1);
        let truncated = total > limit;
        let candidates = resources
            .iter()
            .take(limit)
            .map(|resource| candidate(resource, &request.query, false))
            .collect();
        Ok(SearchResponse {
            candidates,
            total,
            truncated,
        })
    }

    pub async fn resolve(&self, request: ResolveRequest) -> ResourceResult<ResolveResponse> {
        let search = SearchRequest {
            query: request.query.clone(),
            kind: request.kind,
            labels: request.labels,
            limit: usize::MAX,
        };
        let mut resources_by_uri = BTreeMap::new();
        for provider in &self.providers {
            for resource in provider
                .search(&search)
                .await?
                .into_iter()
                .filter(|resource| {
                    resource.uri.to_string() == search.query
                        || resource.uri.id() == search.query
                        || resource.name == search.query
                        || resource.aliases.iter().any(|alias| alias == &search.query)
                })
            {
                if provider.is_mutable() {
                    resources_by_uri.insert(resource.uri.clone(), resource);
                } else {
                    resources_by_uri
                        .entry(resource.uri.clone())
                        .or_insert(resource);
                }
            }
        }
        let resources = resources_by_uri.into_values().collect::<Vec<_>>();
        match resources.as_slice() {
            [] => Ok(ResolveResponse {
                status: ResolveStatus::NotFound,
                selected: None,
                candidates: Vec::new(),
                message: "no resource matched query".into(),
            }),
            [resource] => Ok(ResolveResponse {
                status: ResolveStatus::Ok,
                selected: Some(candidate(resource, &search.query, true)),
                candidates: Vec::new(),
                message: "resource resolved".into(),
            }),
            _ => Ok(ResolveResponse {
                status: ResolveStatus::Ambiguous,
                selected: None,
                candidates: resources
                    .iter()
                    .map(|resource| candidate(resource, &search.query, false))
                    .collect(),
                message: "multiple resources matched query".into(),
            }),
        }
    }

    pub async fn describe(&self, request: DescribeRequest) -> ResourceResult<Option<Resource>> {
        let mut first_hit = None;
        for provider in &self.providers {
            if let Some(resource) = provider.describe(&request).await? {
                if provider.is_mutable() {
                    return Ok(Some(resource));
                }
                if first_hit.is_none() {
                    first_hit = Some(resource);
                }
            }
        }
        Ok(first_hit)
    }

    pub async fn validate(&self, request: ValidateRequest) -> ResourceResult<ValidateResponse> {
        let mut failed_response = None;
        for provider in &self.providers {
            let response = provider.validate(&request).await?;
            if response.ok {
                return Ok(response);
            }
            if failed_response.is_none() {
                failed_response = Some(response);
            }
        }
        Ok(failed_response.unwrap_or_else(|| ValidateResponse {
            ok: false,
            message: "resource not found".into(),
            warnings: Vec::new(),
            code: None,
            field: None,
            retryable: None,
            suggested_fix: None,
            details: serde_json::Value::Null,
        }))
    }
}

fn candidate(resource: &Resource, query: &str, include_resource: bool) -> ResourceCandidate {
    let mut matched = Vec::new();
    if resource.uri.to_string() == query || resource.uri.id() == query {
        matched.push("uri".into());
    }
    if resource.name == query {
        matched.push("name".into());
    }
    if resource.aliases.iter().any(|alias| alias == query) {
        matched.push("alias".into());
    }
    if matched.is_empty()
        && resource
            .searchable_text()
            .to_lowercase()
            .contains(&query.to_lowercase())
    {
        matched.push("text".into());
    }
    let score = if matched.iter().any(|item| item == "uri") {
        1.0
    } else {
        0.8
    };
    ResourceCandidate::from_resource(resource, score, matched, include_resource)
}
