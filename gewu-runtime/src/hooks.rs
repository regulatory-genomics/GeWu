//! Runtime plugin hook registry and HTTP hook provider.

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

use crate::manifest::schema::{
    HookErrorPolicy, RuntimeHookEvent, RuntimeHookSpec, RuntimePluginManifest,
};

#[derive(Debug, Clone, Default)]
pub struct HookRegistry {
    providers: Vec<Arc<HttpHookProvider>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    pub fn register(&mut self, provider: HttpHookProvider) {
        self.providers.push(Arc::new(provider));
    }

    pub fn len(&self) -> usize {
        self.providers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    pub async fn call_event(&self, event: RuntimeHookEvent, payload: Value) -> Result<Vec<Value>> {
        let mut responses = Vec::new();
        for provider in &self.providers {
            let provider_responses = provider.call(event, payload.clone()).await?;
            responses.extend(provider_responses);
        }
        Ok(responses)
    }

    pub async fn transform_system_prompt(
        &self,
        mut prompt: String,
        payload: Value,
    ) -> Result<String> {
        for response in self
            .call_event(RuntimeHookEvent::SpecialistSystemPromptTransform, payload)
            .await?
        {
            if let Some(replacement) = response
                .pointer("/patch/replace_system_prompt")
                .and_then(Value::as_str)
            {
                prompt = replacement.to_string();
                continue;
            }

            let append = response
                .get("rendered")
                .and_then(Value::as_str)
                .or_else(|| {
                    response
                        .pointer("/patch/append_system_prompt")
                        .and_then(Value::as_str)
                });
            if let Some(append) = append {
                if !append.trim().is_empty() {
                    prompt.push_str("\n\n");
                    prompt.push_str(append);
                }
            }
        }
        Ok(prompt)
    }
}

#[derive(Debug, Clone)]
pub struct HttpHookProvider {
    id: String,
    base_url: String,
    timeout_ms: u64,
    default_on_error: HookErrorPolicy,
    hooks: Vec<RuntimeHookSpec>,
    client: reqwest::Client,
}

impl HttpHookProvider {
    pub fn from_runtime_plugin(manifest: &RuntimePluginManifest) -> Result<Option<Self>> {
        if manifest.hooks.is_empty() {
            return Ok(None);
        }
        let base_url = manifest
            .base_url
            .clone()
            .ok_or_else(|| anyhow!("runtime plugin '{}' is missing base_url", manifest.id))?;
        Ok(Some(Self {
            id: manifest.id.clone(),
            base_url: base_url.trim_end_matches('/').to_string(),
            timeout_ms: manifest.timeout_ms.unwrap_or(1500),
            default_on_error: manifest.on_error,
            hooks: manifest.hooks.clone(),
            client: reqwest::Client::new(),
        }))
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn events(&self) -> Vec<RuntimeHookEvent> {
        self.hooks.iter().map(|hook| hook.event).collect()
    }

    async fn call(&self, event: RuntimeHookEvent, payload: Value) -> Result<Vec<Value>> {
        let mut responses = Vec::new();
        for hook in self.hooks.iter().filter(|hook| hook.event == event) {
            match self.call_hook(hook, payload.clone()).await {
                Ok(response) => responses.push(response),
                Err(err)
                    if hook.on_error == HookErrorPolicy::Continue
                        || self.default_on_error == HookErrorPolicy::Continue =>
                {
                    tracing::warn!(
                        "Runtime hook '{}:{}' failed and was ignored: {}",
                        self.id,
                        hook.name,
                        err
                    );
                }
                Err(err) => return Err(err),
            }
        }
        Ok(responses)
    }

    async fn call_hook(&self, hook: &RuntimeHookSpec, payload: Value) -> Result<Value> {
        let url = format!("{}{}", self.base_url, normalize_endpoint(&hook.endpoint));
        let timeout = Duration::from_millis(hook.timeout_ms.unwrap_or(self.timeout_ms));
        let method = hook.method.to_uppercase();
        let request = match method.as_str() {
            "POST" => self.client.post(&url).json(&payload),
            "PUT" => self.client.put(&url).json(&payload),
            "PATCH" => self.client.patch(&url).json(&payload),
            "GET" => self.client.get(&url).query(&payload),
            other => return Err(anyhow!("unsupported hook HTTP method: {}", other)),
        };
        let response = request
            .timeout(timeout)
            .send()
            .await
            .with_context(|| format!("calling runtime hook {} at {}", hook.name, url))?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(anyhow!(
                "runtime hook {} returned {}: {}",
                hook.name,
                status,
                body
            ));
        }
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body)
            .with_context(|| format!("parsing response from hook {}", hook.name))
    }
}

fn normalize_endpoint(endpoint: &str) -> String {
    if endpoint.starts_with('/') {
        endpoint.to_string()
    } else {
        format!("/{}", endpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transform_system_prompt_appends_rendered_response() {
        let registry = HookRegistry::new();
        let prompt = registry
            .transform_system_prompt("base".to_string(), serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(prompt, "base");
    }

    #[test]
    fn http_hook_provider_loads_runtime_plugin_hooks() {
        let manifest: RuntimePluginManifest = serde_yaml::from_str(
            r#"
id: gewu-memory
kind: http
base_url: http://127.0.0.1:7878
hooks:
  - name: prepare_context
    event: specialist.system_prompt.transform
    endpoint: /v1/context/prepare
"#,
        )
        .unwrap();

        let provider = HttpHookProvider::from_runtime_plugin(&manifest)
            .unwrap()
            .unwrap();
        assert_eq!(provider.id(), "gewu-memory");
        assert_eq!(
            provider.events(),
            vec![RuntimeHookEvent::SpecialistSystemPromptTransform]
        );
    }
}
