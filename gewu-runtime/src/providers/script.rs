//! Script tool provider — runs a script (shell/python/node) as a tool.

use gewu_core::traits::{AssemblyContext, Tool, ToolProvider};
use gewu_core::types::{CapabilityDescriptor, ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptProviderSpec {
    pub id: String,
    pub capabilities: Vec<ScriptCapabilitySpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptCapabilitySpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_runtime")]
    pub runtime: String,
    pub entry_point: String,
    #[serde(default)]
    pub input_schema: Value,
}

fn default_runtime() -> String {
    "bash".into()
}

pub struct ScriptToolProvider {
    spec: ScriptProviderSpec,
}

impl ScriptToolProvider {
    pub fn from_spec(spec: ScriptProviderSpec) -> Self {
        Self { spec }
    }
}

impl ToolProvider for ScriptToolProvider {
    fn kind(&self) -> &str {
        "script"
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
                provider_kind: "script".into(),
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
            .ok_or_else(|| format!("unknown script capability: {capability}"))?;
        Ok(vec![Box::new(ScriptTool { cap: cap.clone() })])
    }
}

struct ScriptTool {
    cap: ScriptCapabilitySpec,
}

#[async_trait::async_trait]
impl Tool for ScriptTool {
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
        let (cmd, cmd_args): (&str, Vec<String>) = match self.cap.runtime.as_str() {
            "bash" | "sh" => ("bash", vec![self.cap.entry_point.clone()]),
            "python" | "python3" => ("python3", vec![self.cap.entry_point.clone()]),
            "node" => ("node", vec![self.cap.entry_point.clone()]),
            other => (other, vec![self.cap.entry_point.clone()]),
        };

        let input_str = serde_json::to_string(&args)?;
        let output = tokio::process::Command::new(cmd)
            .args(&cmd_args)
            .env("GEWU_INPUT", &input_str)
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(ToolResult::new(&self.cap.name, stdout.to_string()))
    }
}
