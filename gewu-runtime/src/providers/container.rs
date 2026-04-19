//! Container tool provider — runs a command inside a Docker/Podman container.

use gewu_core::traits::{AssemblyContext, Tool, ToolProvider};
use gewu_core::types::{CapabilityDescriptor, ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerProviderSpec {
    pub id: String,
    #[serde(default = "default_engine")]
    pub engine: String,
    pub capabilities: Vec<ContainerCapabilitySpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerCapabilitySpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub image: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub input_schema: Value,
}

fn default_engine() -> String {
    "docker".into()
}

pub struct ContainerToolProvider {
    spec: ContainerProviderSpec,
}

impl ContainerToolProvider {
    pub fn from_spec(spec: ContainerProviderSpec) -> Self {
        Self { spec }
    }
}

impl ToolProvider for ContainerToolProvider {
    fn kind(&self) -> &str {
        "container"
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
                provider_kind: "container".into(),
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
            .ok_or_else(|| format!("unknown container capability: {}", capability))?;
        Ok(vec![Box::new(ContainerTool {
            engine: self.spec.engine.clone(),
            cap: cap.clone(),
        })])
    }
}

struct ContainerTool {
    engine: String,
    cap: ContainerCapabilitySpec,
}

#[async_trait::async_trait]
impl Tool for ContainerTool {
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
        let input_str = serde_json::to_string(&args)?;
        let mut cmd_args = vec![
            "run".to_string(),
            "--rm".to_string(),
            "-e".to_string(),
            format!("GEWU_INPUT={}", input_str),
            self.cap.image.clone(),
        ];
        cmd_args.extend(self.cap.command.clone());

        let output = tokio::process::Command::new(&self.engine)
            .args(&cmd_args)
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(ToolResult::new(&self.cap.name, stdout.to_string()))
    }
}
