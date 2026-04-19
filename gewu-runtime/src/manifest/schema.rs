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
}
