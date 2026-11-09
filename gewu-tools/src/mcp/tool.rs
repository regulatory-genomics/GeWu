use crate::mcp::client::McpClient;
use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde_json::Value;
use std::error::Error;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct McpToolWrapper {
    pub client: Arc<Mutex<McpClient>>,
    pub name: String,
    pub description: String,
    pub schema: RootSchema,
}

#[async_trait]
impl Tool for McpToolWrapper {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> RootSchema {
        self.schema.clone()
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let client = self.client.clone();

        let client_guard = client.lock().await;

        let result = client_guard
            .call_tool(&self.name, args)
            .await
            .map_err(|e| {
                let s = e.to_string();
                Box::new(std::io::Error::other(s))
                    as Box<dyn std::error::Error + Send + Sync>
            })?;

        Ok(ToolResult::new(&self.name, result))
    }
}
