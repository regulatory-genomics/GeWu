//! CLI tool provider — wraps an external command as a tool.

use gewu_core::traits::{AssemblyContext, Tool, ToolProvider};
use gewu_core::types::{CapabilityDescriptor, ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliProviderSpec {
    pub id: String,
    pub capabilities: Vec<CliCapabilitySpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliCapabilitySpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub input_schema: Value,
}

pub struct CliToolProvider {
    spec: CliProviderSpec,
}

impl CliToolProvider {
    pub fn from_spec(spec: CliProviderSpec) -> Self {
        Self { spec }
    }
}

impl ToolProvider for CliToolProvider {
    fn kind(&self) -> &str {
        "cli"
    }

    fn id(&self) -> &str {
        &self.spec.id
    }

    fn capabilities(&self) -> Vec<CapabilityDescriptor> {
        self.spec
            .capabilities
            .iter()
            .map(|c| CapabilityDescriptor {
                name: c.name.clone(),
                description: c.description.clone(),
                provider_kind: "cli".into(),
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
            .spec
            .capabilities
            .iter()
            .find(|c| c.name == capability)
            .ok_or_else(|| format!("unknown cli capability: {}", capability))?;
        Ok(vec![Box::new(CliTool {
            cap: cap.clone(),
        })])
    }
}

struct CliTool {
    cap: CliCapabilitySpec,
}

#[async_trait::async_trait]
impl Tool for CliTool {
    fn name(&self) -> &str {
        &self.cap.name
    }

    fn description(&self) -> &str {
        &self.cap.description
    }

    fn schema(&self) -> RootSchema {
        if let Ok(schema) = serde_json::from_value(self.cap.input_schema.clone()) {
            schema
        } else {
            RootSchema::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let mut cmd_args = self.cap.args.clone();
        if let Some(input) = args.get("input").and_then(|v| v.as_str()) {
            cmd_args.push(input.to_string());
        }

        let output = tokio::process::Command::new(&self.cap.command)
            .args(&cmd_args)
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = if stderr.is_empty() {
            stdout.to_string()
        } else {
            format!("{}\n--- stderr ---\n{}", stdout, stderr)
        };

        Ok(ToolResult::new(&self.cap.name, combined))
    }
}
