use std::error::Error;
use std::sync::Arc;

use gewu_core::traits::{AssemblyContext, Tool, ToolFactory};

use crate::provider::MutableResourceProvider;
use crate::registry::ResourceRegistry;
use crate::tools::{
    ResourceDescribeTool, ResourceRegisterTool, ResourceResolveTool, ResourceSearchTool,
    ResourceValidateTool,
};

pub struct ResourceToolFactory {
    registry: Arc<ResourceRegistry>,
    mutable_provider: Option<Arc<MutableResourceProvider>>,
    register_lock: Option<Arc<tokio::sync::Mutex<()>>>,
}

impl ResourceToolFactory {
    pub fn new(registry: Arc<ResourceRegistry>) -> Self {
        Self {
            registry,
            mutable_provider: None,
            register_lock: None,
        }
    }

    pub fn with_registration(
        registry: Arc<ResourceRegistry>,
        mutable_provider: Arc<MutableResourceProvider>,
    ) -> Self {
        Self {
            registry,
            mutable_provider: Some(mutable_provider),
            register_lock: Some(Arc::new(tokio::sync::Mutex::new(()))),
        }
    }
}

impl ToolFactory for ResourceToolFactory {
    fn capabilities(&self) -> Vec<String> {
        let mut capabilities = vec![
            "resource.describe".into(),
            "resource.resolve".into(),
            "resource.search".into(),
            "resource.validate".into(),
        ];
        if self.mutable_provider.is_some() {
            capabilities.push("resource.register".into());
        }
        capabilities
    }

    fn create_tools(
        &self,
        capability: &str,
        ctx: &AssemblyContext,
    ) -> Result<Vec<Box<dyn Tool>>, Box<dyn Error + Send + Sync>> {
        let tool: Option<Box<dyn Tool>> = match capability {
            "resource.describe" => Some(Box::new(ResourceDescribeTool::new(self.registry.clone()))),
            "resource.resolve" => Some(Box::new(ResourceResolveTool::new(self.registry.clone()))),
            "resource.search" => Some(Box::new(ResourceSearchTool::new(self.registry.clone()))),
            "resource.validate" => Some(Box::new(ResourceValidateTool::new(self.registry.clone()))),
            "resource.register" => self.mutable_provider.as_ref().map(|provider| {
                Box::new(ResourceRegisterTool::with_lock(
                    self.registry.clone(),
                    provider.clone(),
                    ctx.working_directory.clone(),
                    self.register_lock
                        .as_ref()
                        .expect("registration lock exists when registration provider exists")
                        .clone(),
                )) as Box<dyn Tool>
            }),
            _ => None,
        };
        Ok(tool.into_iter().collect())
    }
}
