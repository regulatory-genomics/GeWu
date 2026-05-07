use std::error::Error;
use std::sync::Arc;

use gewu_core::traits::{AssemblyContext, Tool, ToolFactory};

use crate::registry::ResourceRegistry;
use crate::tools::{
    ResourceDescribeTool, ResourceResolveTool, ResourceSearchTool, ResourceValidateTool,
};

pub struct ResourceToolFactory {
    registry: Arc<ResourceRegistry>,
}

impl ResourceToolFactory {
    pub fn new(registry: Arc<ResourceRegistry>) -> Self {
        Self { registry }
    }
}

impl ToolFactory for ResourceToolFactory {
    fn capabilities(&self) -> Vec<String> {
        vec![
            "resource.describe".into(),
            "resource.resolve".into(),
            "resource.search".into(),
            "resource.validate".into(),
        ]
    }

    fn create_tools(
        &self,
        capability: &str,
        _ctx: &AssemblyContext,
    ) -> Result<Vec<Box<dyn Tool>>, Box<dyn Error + Send + Sync>> {
        let tool: Option<Box<dyn Tool>> = match capability {
            "resource.describe" => Some(Box::new(ResourceDescribeTool::new(self.registry.clone()))),
            "resource.resolve" => Some(Box::new(ResourceResolveTool::new(self.registry.clone()))),
            "resource.search" => Some(Box::new(ResourceSearchTool::new(self.registry.clone()))),
            "resource.validate" => Some(Box::new(ResourceValidateTool::new(self.registry.clone()))),
            _ => None,
        };
        Ok(tool.into_iter().collect())
    }
}
