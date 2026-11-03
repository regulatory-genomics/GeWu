use anyhow::Result;
use gewu_core::traits::Tool;
use gewu_tools::mcp::types::McpServerConfig;
use gewu_tools::mcp::{McpClient, McpToolWrapper};
use schemars::schema::RootSchema;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};

pub async fn load_mcp_tools(
    mcp_servers: HashMap<String, McpServerConfig>,
) -> Result<Vec<Arc<dyn Tool>>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();

    for (server_name, server_config) in mcp_servers {
        info!("Connecting to MCP server: {}", server_name);

        match McpClient::connect(&server_config).await {
            Ok(mut client) => {
                if let Err(e) = client.initialize().await {
                    warn!("Failed to initialize MCP server '{}': {}", server_name, e);
                    continue;
                }

                let mcp_tools = match client.list_tools().await {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(
                            "Failed to list tools from MCP server '{}': {}",
                            server_name, e
                        );
                        continue;
                    }
                };

                info!("Loaded {} tools from '{}'", mcp_tools.len(), server_name);

                let client_arc = Arc::new(Mutex::new(client));

                for tool in mcp_tools {
                    let schema: Result<RootSchema, _> =
                        serde_json::from_value(tool.input_schema.clone());
                    let schema = schema.unwrap_or_else(|_| {
                        serde_json::from_value(serde_json::json!({
                            "type": "object",
                            "properties": {}
                        }))
                        .unwrap()
                    });
                    tools.push(Arc::new(McpToolWrapper {
                        client: client_arc.clone(),
                        name: tool.name,
                        description: tool.description.unwrap_or_default(),
                        schema,
                    }) as Arc<dyn Tool>);
                }
            }
            Err(e) => {
                warn!("Failed to connect to MCP server '{}': {}", server_name, e);
            }
        }
    }

    Ok(tools)
}
