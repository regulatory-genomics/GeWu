//! HTTP tool provider — wraps a REST API endpoint as a tool.

use gewu_core::traits::{AssemblyContext, Tool, ToolProvider};
use gewu_core::types::{CapabilityDescriptor, ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::schema::RuntimePluginManifest;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpProviderSpec {
    pub id: String,
    pub capabilities: Vec<HttpCapabilitySpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpCapabilitySpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub input_schema: Value,
}

fn default_method() -> String {
    "POST".into()
}

pub struct HttpToolProvider {
    spec: HttpProviderSpec,
}

impl HttpToolProvider {
    pub fn from_spec(spec: HttpProviderSpec) -> Self {
        Self { spec }
    }

    pub fn from_runtime_plugin(
        manifest: &RuntimePluginManifest,
    ) -> Result<Option<Self>, Box<dyn std::error::Error + Send + Sync>> {
        if manifest.tools.is_empty() {
            return Ok(None);
        }
        let base_url = manifest
            .base_url
            .as_deref()
            .ok_or_else(|| format!("runtime plugin '{}' is missing base_url", manifest.id))?;
        let base_url = base_url.trim_end_matches('/');
        let capabilities = manifest
            .tools
            .iter()
            .map(|tool| HttpCapabilitySpec {
                name: tool.name.clone(),
                description: tool.description.clone(),
                url: format!("{}{}", base_url, normalize_endpoint(&tool.endpoint)),
                method: tool.method.clone(),
                headers: std::collections::HashMap::new(),
                input_schema: tool.input_schema.clone(),
            })
            .collect();
        Ok(Some(Self::from_spec(HttpProviderSpec {
            id: manifest.id.clone(),
            capabilities,
        })))
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

    #[test]
    fn runtime_plugin_tools_convert_to_http_capabilities() {
        let manifest: RuntimePluginManifest = serde_yaml::from_str(
            r#"
id: gewu-memory
kind: http
base_url: http://127.0.0.1:7878
tools:
  - name: memory_search
    description: Search prior memory
    endpoint: /v1/memory/search
    method: POST
"#,
        )
        .unwrap();

        let provider = HttpToolProvider::from_runtime_plugin(&manifest)
            .unwrap()
            .unwrap();
        let caps = provider.capabilities();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].name, "memory_search");
        assert_eq!(caps[0].provider_kind, "http");
    }
}

impl ToolProvider for HttpProviderSpec {
    fn kind(&self) -> &str {
        "http"
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn capabilities(&self) -> Vec<CapabilityDescriptor> {
        self.capabilities
            .iter()
            .map(|c| CapabilityDescriptor {
                name: c.name.clone(),
                description: c.description.clone(),
                provider_kind: "http".into(),
                input_schema: c.input_schema.clone(),
                metadata: Value::Null,
            })
            .collect()
    }
    fn build_tools(
        &self,
        capability: &str,
        _ctx: &AssemblyContext,
    ) -> Result<Vec<Box<dyn Tool>>, Box<dyn std::error::Error + Send + Sync>> {
        let cap = self
            .capabilities
            .iter()
            .find(|c| c.name == capability)
            .ok_or_else(|| format!("unknown http capability: {}", capability))?;
        Ok(vec![Box::new(HttpTool { cap: cap.clone() })])
    }
}

impl ToolProvider for HttpToolProvider {
    fn kind(&self) -> &str {
        "http"
    }
    fn id(&self) -> &str {
        &self.spec.id
    }
    fn capabilities(&self) -> Vec<CapabilityDescriptor> {
        self.spec.capabilities()
    }
    fn build_tools(
        &self,
        capability: &str,
        ctx: &AssemblyContext,
    ) -> Result<Vec<Box<dyn Tool>>, Box<dyn std::error::Error + Send + Sync>> {
        self.spec.build_tools(capability, ctx)
    }
}

struct HttpTool {
    cap: HttpCapabilitySpec,
}

#[async_trait::async_trait]
impl Tool for HttpTool {
    fn name(&self) -> &str {
        &self.cap.name
    }
    fn description(&self) -> &str {
        &self.cap.description
    }
    fn schema(&self) -> RootSchema {
        serde_json::from_value(self.cap.input_schema.clone()).unwrap_or_default()
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let client = reqwest::Client::new();
        let mut url = self.cap.url.clone();
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                let placeholder = format!("{{{}}}", k);
                if url.contains(&placeholder) {
                    let val = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string().trim_matches('"').to_string(),
                    };
                    url = url.replace(&placeholder, &val);
                }
            }
        }

        let mut req = match self.cap.method.to_uppercase().as_str() {
            "GET" => client.get(&url),
            "POST" => client.post(&url),
            "PUT" => client.put(&url),
            "DELETE" => client.delete(&url),
            "PATCH" => client.patch(&url),
            m => return Err(format!("unsupported HTTP method: {}", m).into()),
        };

        for (k, v) in &self.cap.headers {
            req = req.header(k.as_str(), v.as_str());
        }

        if self.cap.method.to_uppercase() == "GET" {
            if let Some(obj) = args.as_object() {
                let pairs: Vec<(String, String)> = obj
                    .iter()
                    .filter_map(|(k, v)| {
                        let placeholder = format!("{{{}}}", k);
                        if self.cap.url.contains(&placeholder) {
                            return None;
                        }
                        let s = match v {
                            serde_json::Value::String(s) => s.clone(),
                            serde_json::Value::Null => return None,
                            other => other.to_string(),
                        };
                        Some((k.clone(), s))
                    })
                    .collect();
                if !pairs.is_empty() {
                    req = req.query(&pairs);
                }
            }
        } else {
            req = req.json(&args);
        }

        let resp = req.send().await?;
        let status = resp.status();
        let body = resp.text().await?;

        Ok(ToolResult::new(
            &self.cap.name,
            format!("[{}] {}", status.as_u16(), body),
        ))
    }
}
