use std::error::Error;
use std::sync::Arc;

use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolExecutionPolicy, ToolResult};
use schemars::{schema::RootSchema, schema_for};
use serde::Serialize;
use serde_json::{Value, json};

use crate::query::{DescribeRequest, ResolveRequest, SearchRequest, ValidateRequest};
use crate::registry::ResourceRegistry;

fn success_result(
    tool_name: &str,
    data: impl Serialize,
) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
    Ok(ToolResult::new(
        tool_name,
        serde_json::to_string(&json!({ "ok": true, "data": data }))?,
    ))
}

fn recoverable_error_result(
    tool_name: &str,
    code: &str,
    message: impl Into<String>,
    suggested_fix: &str,
) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
    Ok(ToolResult::new(
        tool_name,
        serde_json::to_string(&json!({
            "ok": false,
            "error": {
                "code": code,
                "retryable": false,
                "message": message.into(),
                "suggested_fix": suggested_fix,
            }
        }))?,
    ))
}

macro_rules! resource_tool {
    ($name:ident, $request:ty, $tool_name:literal, $description:literal, $method:ident) => {
        pub struct $name {
            registry: Arc<ResourceRegistry>,
        }

        impl $name {
            pub fn new(registry: Arc<ResourceRegistry>) -> Self {
                Self { registry }
            }
        }

        #[async_trait]
        impl Tool for $name {
            fn name(&self) -> &str {
                $tool_name
            }

            fn description(&self) -> &str {
                $description
            }

            fn schema(&self) -> RootSchema {
                schema_for!($request)
            }

            fn execution_policy(&self) -> ToolExecutionPolicy {
                ToolExecutionPolicy::ParallelSafe
            }

            async fn execute(
                &self,
                args: Value,
                _ctx: &ToolContext,
            ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
                let request: $request = match serde_json::from_value(args) {
                    Ok(request) => request,
                    Err(error) => {
                        return recoverable_error_result(
                            $tool_name,
                            "invalid_args",
                            error.to_string(),
                            "Call this tool with arguments matching its JSON schema.",
                        );
                    }
                };
                let response = match self.registry.$method(request).await {
                    Ok(response) => response,
                    Err(error) => {
                        return recoverable_error_result(
                            $tool_name,
                            "resource_error",
                            error.to_string(),
                            "Check the resource registry configuration and retry with a valid resource query.",
                        );
                    }
                };
                success_result($tool_name, response)
            }
        }
    };
}

resource_tool!(
    ResourceSearchTool,
    SearchRequest,
    "resource.search",
    "Search registered resources",
    search
);
resource_tool!(
    ResourceResolveTool,
    ResolveRequest,
    "resource.resolve",
    "Resolve a resource query to one resource or candidates",
    resolve
);
resource_tool!(
    ResourceDescribeTool,
    DescribeRequest,
    "resource.describe",
    "Describe a resource by URI",
    describe
);
resource_tool!(
    ResourceValidateTool,
    ValidateRequest,
    "resource.validate",
    "Validate that a resource value is usable",
    validate
);
