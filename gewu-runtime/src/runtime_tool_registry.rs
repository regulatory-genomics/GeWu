use async_trait::async_trait;
use gewu_core::traits::{Tool, ToolRegistry};
use gewu_core::types::{ToolContext, ToolResult};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone)]
pub struct RuntimeToolRegistry {
    tools: Arc<Vec<Arc<dyn Tool>>>,
}

#[derive(Clone)]
pub struct RuntimeToolRegistryFactory;

impl RuntimeToolRegistry {
    pub fn new(tools: impl Into<Arc<Vec<Arc<dyn Tool>>>>) -> Self {
        Self {
            tools: tools.into(),
        }
    }

    pub fn from_vec(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self::new(tools)
    }
}

#[async_trait]
impl ToolRegistry for RuntimeToolRegistry {
    async fn execute(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let tool = self.tools.iter().find(|t| t.name() == name);
        match tool {
            Some(t) => t.execute(args, ctx).await,
            None => Err(format!("Tool not found: {name}").into()),
        }
    }

    fn list_tools(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    fn execution_policy(&self, name: &str) -> Option<gewu_core::types::ToolExecutionPolicy> {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(|tool| tool.execution_policy())
    }
}

impl From<Vec<Arc<dyn Tool>>> for RuntimeToolRegistry {
    fn from(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self::from_vec(tools)
    }
}
