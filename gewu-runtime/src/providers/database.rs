//! Database tool provider — wraps a SQL query template as a tool.

use gewu_core::traits::{AssemblyContext, Tool, ToolProvider};
use gewu_core::types::{CapabilityDescriptor, ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseProviderSpec {
    pub id: String,
    pub connection: String,
    pub capabilities: Vec<DatabaseCapabilitySpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseCapabilitySpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub query: String,
    #[serde(default)]
    pub input_schema: Value,
}

pub struct DatabaseToolProvider {
    spec: DatabaseProviderSpec,
}

impl DatabaseToolProvider {
    pub fn from_spec(spec: DatabaseProviderSpec) -> Self {
        Self { spec }
    }
}

impl ToolProvider for DatabaseToolProvider {
    fn kind(&self) -> &str {
        "database"
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
                provider_kind: "database".into(),
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
            .ok_or_else(|| format!("unknown database capability: {}", capability))?;
        Ok(vec![Box::new(DatabaseTool {
            connection: self.spec.connection.clone(),
            cap: cap.clone(),
        })])
    }
}

struct DatabaseTool {
    connection: String,
    cap: DatabaseCapabilitySpec,
}

#[async_trait::async_trait]
impl Tool for DatabaseTool {
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
        // Simple implementation using sqlite3 CLI for now.
        let mut query = self.cap.query.clone();
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                let replacement = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                query = query.replace(&format!(":{}", k), &replacement);
            }
        }

        let output = tokio::process::Command::new("sqlite3")
            .args(["-json", &self.connection, &query])
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(ToolResult::new(&self.cap.name, stdout.to_string()))
    }
}
