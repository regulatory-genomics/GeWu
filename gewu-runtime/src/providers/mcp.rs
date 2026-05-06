//! MCP tool provider — wraps MCP server tools discovered via the MCP protocol.
//!
//! This delegates to the existing `gewu_tools::mcp` layer, parameterized by a
//! YAML-provided server config.

use gewu_core::traits::{AssemblyContext, Tool, ToolProvider};
use gewu_core::types::CapabilityDescriptor;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpProviderSpec {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub capability_names: Vec<String>,
}

pub struct McpToolProvider {
    spec: McpProviderSpec,
}

impl McpToolProvider {
    pub fn from_spec(spec: McpProviderSpec) -> Self {
        Self { spec }
    }
}

impl ToolProvider for McpToolProvider {
    fn kind(&self) -> &str {
        "mcp"
    }

    fn id(&self) -> &str {
        &self.spec.id
    }

    fn capabilities(&self) -> Vec<CapabilityDescriptor> {
        self.spec
            .capability_names
            .iter()
            .map(|name| CapabilityDescriptor::new(name, "mcp"))
            .collect()
    }

    fn build_tools(
        &self,
        _capability: &str,
        _ctx: &AssemblyContext,
    ) -> Result<Vec<Box<dyn Tool>>, Box<dyn std::error::Error + Send + Sync>> {
        // MCP tools are discovered at runtime via the MCP protocol.
        // Full integration deferred to after the MCP handshake layer is wired.
        Err(format!(
            "MCP provider '{}': runtime tool discovery not yet integrated — \
             use the existing mcpServers config path for now",
            self.spec.id
        )
        .into())
    }
}

/// Placeholder: in a future milestone, this will call the MCP handshake,
/// discover tools, and return them. For now, MCP tools are loaded via the
/// existing `mcp_loader` path in gewu-cli.
pub fn _discover_mcp_tools(
    _spec: &McpProviderSpec,
) -> Result<Vec<Box<dyn Tool>>, Box<dyn std::error::Error + Send + Sync>> {
    Err("MCP tool discovery via ToolProvider not yet wired".into())
}
