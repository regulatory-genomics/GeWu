use serde::{Deserialize, Serialize};
use serde_json::Value;

// JSON-RPC 2.0 Base Types

/// Deserializes a JSON-RPC id that may be a number, a numeric string (some
/// servers emit string ids), or null — so string-id responses are correlated
/// instead of silently dropped.
fn flexible_id<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value: Option<Value> = Option::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_u64()),
        Some(Value::String(s)) => s
            .parse::<u64>()
            .map(Some)
            .map_err(|_| D::Error::custom(format!("non-numeric id string: {s}"))),
        Some(other) => Err(D::Error::custom(format!("unsupported id value: {other}"))),
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    pub id: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    #[serde(default, deserialize_with = "flexible_id")]
    pub id: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// MCP Specific Types

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogMessageParams {
    pub level: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logger: Option<String>,
    pub data: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    pub client_info: ClientInfo,
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    // Add supported capabilities here (e.g., roots, sampling, etc.)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    pub server_info: ServerInfo,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    #[serde(default)]
    pub tools: Option<serde_json::Value>, // Can be boolean or object
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

// Tools

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListToolsResult {
    pub tools: Vec<Tool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolParams {
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    pub content: Vec<ToolContent>,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ToolContent {
    Text { text: String },
    Image { data: String, mime_type: String },
    EmbeddedResource { resource: Value },
}

// Config

#[derive(Debug, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(rename = "mcpServers")]
    pub mcp_servers: std::collections::HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
    },
    Sse {
        url: String,
    },
}
