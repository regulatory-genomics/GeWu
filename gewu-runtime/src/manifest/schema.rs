//! YAML schema for specialist manifests and tool provider configs.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Specialist manifest as stored in YAML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecialistManifest {
    pub id: String,
    #[serde(default)]
    pub description: String,
    pub prompt: PromptSource,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub policy: HashMap<String, Value>,
    #[serde(default)]
    pub metadata: Value,
}

/// How to obtain the specialist's system prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PromptSource {
    Inline { inline: String },
    File { file: String },
}

impl PromptSource {
    /// Resolve the prompt text.
    ///
    /// `base` is the directory the manifest YAML lives in (so that relative
    /// `file:` paths resolve correctly).
    pub fn resolve(&self, base: &std::path::Path) -> std::io::Result<String> {
        match self {
            PromptSource::Inline { inline } => Ok(inline.clone()),
            PromptSource::File { file } => {
                let path = if std::path::Path::new(file).is_absolute() {
                    std::path::PathBuf::from(file)
                } else {
                    base.join(file)
                };
                std::fs::read_to_string(&path)
            }
        }
    }
}

/// Tool-provider manifest as stored in YAML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolProviderManifest {
    pub kind: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub capabilities: Vec<ToolProviderCapability>,
    #[serde(flatten)]
    pub spec: Value,
}

/// A single capability entry inside a tool-provider manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolProviderCapability {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
}

/// YAML-configured runtime plugin manifest.
///
/// Runtime plugins are loaded without recompiling GeWu. They can contribute
/// agent-callable tools and lifecycle hooks through an external provider such
/// as an HTTP daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePluginManifest {
    pub id: String,
    pub kind: RuntimePluginKind,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub on_error: HookErrorPolicy,
    #[serde(default)]
    pub tools: Vec<RuntimeToolSpec>,
    #[serde(default)]
    pub hooks: Vec<RuntimeHookSpec>,
    #[serde(flatten)]
    pub spec: Value,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePluginKind {
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub endpoint: String,
    #[serde(default = "default_post")]
    pub method: String,
    #[serde(default)]
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeHookSpec {
    pub name: String,
    pub event: RuntimeHookEvent,
    pub endpoint: String,
    #[serde(default = "default_post")]
    pub method: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub on_error: HookErrorPolicy,
}

fn default_post() -> String {
    "POST".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeHookEvent {
    #[serde(rename = "specialist.system_prompt.transform")]
    SpecialistSystemPromptTransform,
    #[serde(rename = "agent.event")]
    AgentEvent,
    #[serde(rename = "tool.execute.before")]
    ToolExecuteBefore,
    #[serde(rename = "tool.execute.after")]
    ToolExecuteAfter,
}

impl RuntimeHookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SpecialistSystemPromptTransform => "specialist.system_prompt.transform",
            Self::AgentEvent => "agent.event",
            Self::ToolExecuteBefore => "tool.execute.before",
            Self::ToolExecuteAfter => "tool.execute.after",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HookErrorPolicy {
    #[default]
    Continue,
    Fail,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_specialist_manifest_inline_prompt() {
        let yaml = r#"
id: test-specialist
description: A test specialist
prompt:
  inline: "You are a test assistant."
capabilities:
  - read_file
  - bash
"#;
        let m: SpecialistManifest = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(m.id, "test-specialist");
        assert_eq!(m.capabilities, vec!["read_file", "bash"]);
        match &m.prompt {
            PromptSource::Inline { inline } => assert!(inline.contains("test assistant")),
            _ => panic!("expected inline prompt"),
        }
    }

    #[test]
    fn parse_specialist_manifest_file_prompt() {
        let yaml = r#"
id: file-prompt
description: Uses file prompt
prompt:
  file: ./prompts/build.txt
capabilities:
  - read_file
"#;
        let m: SpecialistManifest = serde_yaml::from_str(yaml).unwrap();
        match &m.prompt {
            PromptSource::File { file } => assert_eq!(file, "./prompts/build.txt"),
            _ => panic!("expected file prompt"),
        }
    }

    #[test]
    fn parse_runtime_plugin_manifest() {
        let yaml = r#"
id: gewu-memory
kind: http
enabled: true
base_url: http://127.0.0.1:7878
timeout_ms: 1500
on_error: continue
tools:
  - name: memory_search
    description: Search prior memory
    endpoint: /v1/memory/search
    method: POST
    input_schema:
      type: object
hooks:
  - name: prepare_memory_context
    event: specialist.system_prompt.transform
    endpoint: /v1/context/prepare
"#;
        let m: RuntimePluginManifest = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(m.id, "gewu-memory");
        assert_eq!(m.kind, RuntimePluginKind::Http);
        assert!(m.enabled);
        assert_eq!(m.tools[0].name, "memory_search");
        assert_eq!(
            m.hooks[0].event,
            RuntimeHookEvent::SpecialistSystemPromptTransform
        );
        assert_eq!(m.hooks[0].method, "POST");
    }
}
