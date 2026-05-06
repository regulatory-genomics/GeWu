use crate::mcp::types::*;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;

enum Transport {
    Stdio {
        stdin: ChildStdin,
        _child: Child,
    },
    Sse {
        client: reqwest::Client,
        endpoint: String,
    },
    HttpPost {
        client: reqwest::Client,
        url: String,
    },
}

pub struct McpClient {
    transport: Arc<Mutex<Transport>>,
    // stdout/SSE stream is continuously read in a background task
    // We need a mechanism to correlate requests with responses
    pending_requests:
        Arc<Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<Result<Value>>>>>,
    next_id: Arc<Mutex<u64>>,

    // Server capabilities/info
    pub server_info: Option<ServerInfo>,
    pub capabilities: Option<ServerCapabilities>,
}

impl McpClient {
    pub async fn connect(config: &McpServerConfig) -> Result<Self> {
        match config {
            McpServerConfig::Stdio { command, args, env } => {
                Self::connect_stdio(command, args, env).await
            }
            McpServerConfig::Sse { url } => Self::connect_sse(url).await,
        }
    }

    async fn connect_stdio(
        command: &str,
        args: &[String],
        env: &std::collections::HashMap<String, String>,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd.spawn().context("Failed to spawn MCP server")?;

        let stdin = child.stdin.take().context("Failed to open stdin")?;
        let stdout = child.stdout.take().context("Failed to open stdout")?;

        let pending_requests: Arc<
            Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<Result<Value>>>>,
        > = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let pending_requests_clone = pending_requests.clone();

        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            loop {
                match reader.next_line().await {
                    Ok(Some(line)) => {
                        if let Ok(msg) = serde_json::from_str::<Value>(&line) {
                            Self::handle_incoming_message(msg, &pending_requests_clone).await;
                        }
                    }
                    Ok(None) | Err(_) => {
                        Self::abort_pending_requests(
                            &pending_requests_clone,
                            "MCP Server connection closed",
                        )
                        .await;
                        break;
                    }
                }
            }
        });

        Ok(Self {
            transport: Arc::new(Mutex::new(Transport::Stdio {
                stdin,
                _child: child,
            })),
            pending_requests,
            next_id: Arc::new(Mutex::new(1)),
            server_info: None,
            capabilities: None,
        })
    }

    async fn connect_sse(url: &str) -> Result<Self> {
        let client = reqwest::Client::new();
        let response = client
            .get(url)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .context("Failed to connect to SSE endpoint")?;

        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            // Fallback to HttpPost mode
            // This is for servers like exa that don't support GET /sse
            return Ok(Self {
                transport: Arc::new(Mutex::new(Transport::HttpPost {
                    client,
                    url: url.to_string(),
                })),
                pending_requests: Arc::new(Mutex::new(std::collections::HashMap::new())),
                next_id: Arc::new(Mutex::new(1)),
                server_info: None,
                capabilities: None,
            });
        }

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Failed to connect to SSE endpoint: {}",
                response.status()
            ));
        }

        let pending_requests: Arc<
            Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<Result<Value>>>>,
        > = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let pending_requests_clone = pending_requests.clone();

        // We need to wait for the first event which MUST be 'endpoint'
        // This is a bit tricky with a raw stream. We need to buffer until we find the first event.
        // For simplicity, we'll spawn the reader immediately and use a channel to signal the endpoint back.

        let (endpoint_tx, endpoint_rx) = tokio::sync::oneshot::channel::<Result<String>>();
        let endpoint_tx = Arc::new(Mutex::new(Some(endpoint_tx)));

        tokio::spawn(async move {
            let mut stream = response.bytes_stream();
            let mut buffer = String::new();

            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        if let Ok(s) = std::str::from_utf8(&chunk) {
                            buffer.push_str(s);

                            // Process complete events (double newline)
                            while let Some(idx) = buffer.find("\n\n") {
                                let event_block = buffer[..idx].to_string();
                                buffer = buffer[idx + 2..].to_string();

                                // Parse event block
                                let mut event_type = "message".to_string();
                                let mut data = String::new();

                                for line in event_block.lines() {
                                    if line.starts_with("event: ") {
                                        event_type = line[7..].trim().to_string();
                                    } else if line.starts_with("data: ") {
                                        data.push_str(&line[6..]);
                                    }
                                }

                                if event_type == "endpoint" {
                                    let mut tx_opt = endpoint_tx.lock().await;
                                    if let Some(tx) = tx_opt.take() {
                                        let _ = tx.send(Ok(data.trim().to_string()));
                                    }
                                } else if event_type == "message"
                                    && let Ok(msg) = serde_json::from_str::<Value>(&data)
                                {
                                    Self::handle_incoming_message(msg, &pending_requests_clone)
                                        .await;
                                }
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            Self::abort_pending_requests(&pending_requests_clone, "SSE connection lost").await;
        });

        // Wait for endpoint
        let endpoint = endpoint_rx
            .await
            .context("Failed to receive endpoint event from SSE stream")?
            .context("Invalid endpoint event")?;

        // Resolve relative endpoint if necessary
        let endpoint_url = if endpoint.starts_with("http") {
            endpoint
        } else {
            // Basic join logic
            let base = if url.ends_with('/') {
                &url[..url.len() - 1]
            } else {
                url
            };
            let path = if endpoint.starts_with('/') {
                &endpoint[1..]
            } else {
                &endpoint
            };
            format!("{}/{}", base, path) // Very naive URL join
        };

        Ok(Self {
            transport: Arc::new(Mutex::new(Transport::Sse {
                client,
                endpoint: endpoint_url,
            })),
            pending_requests,
            next_id: Arc::new(Mutex::new(1)),
            server_info: None,
            capabilities: None,
        })
    }

    async fn abort_pending_requests(
        pending: &Arc<
            Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<Result<Value>>>>,
        >,
        reason: &str,
    ) {
        let mut p = pending.lock().await;
        for (_, sender) in p.drain() {
            if sender.send(Err(anyhow::anyhow!("{}", reason))).is_err() {
                tracing::debug!("MCP: channel already closed when aborting pending request");
            }
        }
    }

    async fn handle_incoming_message(
        msg: Value,
        pending_requests: &Arc<
            Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<Result<Value>>>>,
        >,
    ) {
        if let Ok(response) = serde_json::from_value::<JsonRpcResponse>(msg.clone())
            && let Some(id) = response.id
        {
            let mut pending = pending_requests.lock().await;
            if let Some(sender) = pending.remove(&id) {
                if let Some(error) = response.error {
                    if sender
                        .send(Err(anyhow::anyhow!(
                            "RPC Error {}: {}",
                            error.code,
                            error.message
                        )))
                        .is_err()
                    {
                        tracing::warn!("MCP: failed to send RPC error response to channel");
                    }
                } else if let Some(result) = response.result {
                    if sender.send(Ok(result)).is_err() {
                        tracing::warn!("MCP: failed to send RPC result to channel");
                    }
                } else if sender.send(Ok(Value::Null)).is_err() {
                    tracing::warn!("MCP: failed to send RPC null result to channel");
                }
            }
            return;
        }

        if let Ok(notification) = serde_json::from_value::<JsonRpcNotification>(msg) {
            if notification.method == "notifications/message" {
                if let Some(params) = notification.params
                    && let Ok(log_params) = serde_json::from_value::<LogMessageParams>(params)
                {
                    let logger = log_params.logger.unwrap_or_else(|| "server".to_string());
                    match log_params.level.as_str() {
                        "error" | "critical" => {
                            tracing::error!(target: "mcp", "[{}] {}", logger, log_params.data)
                        }
                        "warning" | "warn" => {
                            tracing::warn!(target: "mcp", "[{}] {}", logger, log_params.data)
                        }
                        "info" => {
                            tracing::info!(target: "mcp", "[{}] {}", logger, log_params.data)
                        }
                        "debug" => {
                            tracing::debug!(target: "mcp", "[{}] {}", logger, log_params.data)
                        }
                        _ => tracing::info!(target: "mcp", "[{}] {}", logger, log_params.data),
                    }
                }
            } else {
                tracing::debug!(target: "mcp", "Received notification: {}", notification.method);
            }
        }
    }

    pub async fn initialize(&mut self) -> Result<()> {
        let params = InitializeParams {
            protocol_version: "2024-11-05".to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo {
                name: "gewu".to_string(),
                version: "0.1.0".to_string(),
            },
        };

        let result = self
            .request("initialize", serde_json::to_value(params)?)
            .await?;
        let init_result: InitializeResult = serde_json::from_value(result)?;

        self.server_info = Some(init_result.server_info);
        self.capabilities = Some(init_result.capabilities);

        // Send initialized notification
        self.notify("notifications/initialized", Value::Null)
            .await?;

        Ok(())
    }

    pub async fn list_tools(&self) -> Result<Vec<Tool>> {
        let result = self.request("tools/list", Value::Null).await?;
        let list_result: ListToolsResult = serde_json::from_value(result)?;
        Ok(list_result.tools)
    }

    pub async fn call_tool(&self, name: &str, args: Value) -> Result<String> {
        let params = CallToolParams {
            name: name.to_string(),
            arguments: args,
        };

        let result_val = self
            .request("tools/call", serde_json::to_value(params)?)
            .await?;
        let result: CallToolResult = serde_json::from_value(result_val)?;

        if result.is_error {
            return Err(anyhow::anyhow!("Tool execution failed"));
        }

        // Combine text content
        let mut output = String::new();
        for content in result.content {
            match content {
                ToolContent::Text { text } => output.push_str(&text),
                _ => output.push_str("[Non-text content]"),
            }
        }
        Ok(output)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = {
            let mut next_id = self.next_id.lock().await;
            let id = *next_id;
            *next_id += 1;
            id
        };

        let mut request_map = serde_json::Map::new();
        request_map.insert("jsonrpc".to_string(), json!("2.0"));
        request_map.insert("method".to_string(), json!(method));

        if !params.is_null() {
            request_map.insert("params".to_string(), params);
        }

        request_map.insert("id".to_string(), json!(id));

        let request = Value::Object(request_map);

        let (tx, rx) = tokio::sync::oneshot::channel();

        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(id, tx);
        }

        let write_result = async {
            let mut transport = self.transport.lock().await;
            match &mut *transport {
                Transport::Stdio { stdin, .. } => {
                    let json_str = serde_json::to_string(&request)?;
                    stdin.write_all(json_str.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                    Ok(None)
                }
                Transport::Sse { client, endpoint } => {
                    let response = client.post(endpoint.as_str()).json(&request).send().await?;
                    if !response.status().is_success() {
                        return Err(anyhow::anyhow!(
                            "Failed to send request via SSE: {}",
                            response.status()
                        ));
                    }
                    Ok(None)
                }
                Transport::HttpPost { client, url } => {
                    let response = client
                        .post(url.as_str())
                        .header("Accept", "application/json, text/event-stream")
                        .header("Content-Type", "application/json")
                        .json(&request)
                        .send()
                        .await?;

                    if !response.status().is_success() {
                        // Parse error body if possible
                        let status = response.status();
                        let text = response.text().await.unwrap_or_default();
                        return Err(anyhow::anyhow!(
                            "Failed to send request via HttpPost: {} - {}",
                            status,
                            text
                        ));
                    }

                    // We need to parse the response here and resolve the pending request.
                    // The response body is SSE stream containing the result.

                    // Read body as text for simplicity as these are usually short.
                    // In a true SSE stream, we should stream it, but for a single response it's likely buffered.
                    let body_text = response.text().await?;

                    // Simple SSE parser for "event: message\ndata: {JSON}"
                    let lines = body_text.lines();
                    let mut current_event = String::new();
                    let mut current_data = String::new();

                    for line in lines {
                        if line.starts_with("event: ") {
                            current_event = line[7..].trim().to_string();
                        } else if line.starts_with("data: ") {
                            current_data.push_str(&line[6..]);
                        } else if line.is_empty() {
                            // End of event block
                            if current_event == "message"
                                && !current_data.is_empty()
                                && let Ok(msg) = serde_json::from_str::<Value>(&current_data)
                            {
                                // We need to handle this message to resolve the pending request
                                // Since we are inside the write_result block which holds the transport lock,
                                // we must NOT call self.handle_incoming_message directly if it tries to lock transport again (it doesn't seem to).
                                // However, handle_incoming_message needs pending_requests which we don't hold lock on here (we released it before write_result).
                                // But we passed pending_requests to handle_incoming_message, so it will lock it internally.
                                // Wait, handle_incoming_message is static/async but needs pending_requests.

                                // We can't call handle_incoming_message easily because it's an instance method or static?
                                // It's static: async fn handle_incoming_message(msg, pending_requests)

                                // We need access to pending_requests.
                                // But we are inside an async block that captures `self`? No, `self` is outside.
                                // The async block captures `transport` lock guard? No, it captures `transport` via `self.transport.lock().await`.

                                // Actually, we can just return the value here?
                                // No, the architecture is designed around `pending_requests` map and channel.
                                // So we must use the channel to send the result back.

                                // We need a reference to pending_requests.
                                // We can clone the arc before the async block.
                                // But wait, `write_result` is executed immediately in the same scope.

                                // The issue is `handle_incoming_message` is not available inside this block easily without passing dependencies.
                                // Let's copy the logic or extract it.

                                if let Ok(response) =
                                    serde_json::from_value::<JsonRpcResponse>(msg.clone())
                                    && let Some(resp_id) = response.id
                                    && resp_id == id
                                {
                                    // Found our response!
                                    // We can return it directly?
                                    // The `request` function waits on `rx`.
                                    // If we resolve it here, `rx` will receive it.

                                    // We need to remove from pending_requests?
                                    // `handle_incoming_message` does that.

                                    // We can't easily access pending_requests inside this match arm because `transport` is locked.
                                    // We can return the message from this block and handle it after releasing transport lock?
                                    return Ok(Some(msg));
                                }
                                // Handle other messages (notifications, logs) if necessary?
                                // For HttpPost request-response, usually we only get the result.
                            }
                            current_event.clear();
                            current_data.clear();
                        }
                    }

                    Ok(None)
                }
            }
        }
        .await;

        match write_result {
            Ok(Some(msg)) => {
                let mut pending = self.pending_requests.lock().await;
                if let Some(sender) = pending.remove(&id)
                    && let Ok(response) = serde_json::from_value::<JsonRpcResponse>(msg)
                {
                    if let Some(error) = response.error {
                        if sender
                            .send(Err(anyhow::anyhow!(
                                "RPC Error {}: {}",
                                error.code,
                                error.message
                            )))
                            .is_err()
                        {
                            tracing::warn!("MCP: failed to send RPC error response to channel");
                        }
                    } else if let Some(result) = response.result {
                        if sender.send(Ok(result)).is_err() {
                            tracing::warn!("MCP: failed to send RPC result to channel");
                        }
                    } else if sender.send(Ok(Value::Null)).is_err() {
                        tracing::warn!("MCP: failed to send RPC null result to channel");
                    }
                }
            }
            Ok(None) => {
                // Standard case or no direct response found
            }
            Err(e) => {
                let mut pending = self.pending_requests.lock().await;
                pending.remove(&id);
                return Err(e);
            }
        }

        // Wait for response with timeout?
        // Let's assume tool calls might take long, so maybe a generous timeout or none.
        let response = rx
            .await
            .context("Channel closed before response received")??;

        Ok(response)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let mut request_map = serde_json::Map::new();
        request_map.insert("jsonrpc".to_string(), json!("2.0"));
        request_map.insert("method".to_string(), json!(method));

        if !params.is_null() {
            request_map.insert("params".to_string(), params);
        }

        let request = Value::Object(request_map);

        let mut transport = self.transport.lock().await;
        match &mut *transport {
            Transport::Stdio { stdin, .. } => {
                let json_str = serde_json::to_string(&request)?;
                stdin.write_all(json_str.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await?;
            }
            Transport::Sse { client, endpoint } => {
                let response = client.post(endpoint.as_str()).json(&request).send().await?;
                if !response.status().is_success() {
                    return Err(anyhow::anyhow!(
                        "Failed to send notification via SSE: {}",
                        response.status()
                    ));
                }
            }
            Transport::HttpPost { client, url } => {
                let response = client
                    .post(url.as_str())
                    .header("Accept", "application/json, text/event-stream")
                    .header("Content-Type", "application/json")
                    .json(&request)
                    .send()
                    .await?;

                if !response.status().is_success() {
                    return Err(anyhow::anyhow!(
                        "Failed to send notification via HttpPost: {}",
                        response.status()
                    ));
                }
            }
        }
        Ok(())
    }
}
