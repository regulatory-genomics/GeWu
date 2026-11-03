//! Server mode for gewu-server.
//!
//! Lets other programs connect to a running GeWu agent, send prompts, and
//! receive streamed agent events. Supports two transports:
//!
//! - **WebSocket** (port 8420): text frames, one JSON message per frame.
//! - **TCP** (port 8421): newline-delimited JSON, one message per line.
//!
//! Both transports share the same protocol (see [`protocol`]) and the same
//! session model: each connection gets an agent created lazily on its first
//! session-needing request and reused for the life of the connection, so
//! conversation context persists across requests on that connection.
//! Clients may pass an explicit `session_id` (e.g. one produced by a
//! previous CLI run) to resume an existing conversation.
//!
//! One `ask` runs at a time per connection. While an ask is running the
//! connection can still send `interrupt`, which aborts the running task
//! and patches the conversation history so it stays consistent.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use gewu_core::ToolRegistry;
use gewu_core::types::{AgentEvent, Message};
pub use gewu_protocol as protocol;
use registry::create_agent;
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};
use tokio::task::{JoinHandle, JoinSet};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{info, warn};

use crate::bootstrap::Bootstrap;
use crate::registry;
pub use protocol::{ClientRequest, Op, RequestError, ServerMessage};

/// Default bind address for both listeners.
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// Default port for the WebSocket listener.
pub const DEFAULT_WS_PORT: u16 = 8420;
/// Default port for the TCP listener.
pub const DEFAULT_TCP_PORT: u16 = 8421;

/// Default agent profile used when the client does not specify one.
const DEFAULT_AGENT: &str = "build";

/// Shared server context handed to each connection handler.
struct ServerState {
    bootstrap: Bootstrap,
}

/// Per-connection session: one agent plus its identity.
#[derive(Clone)]
struct ConnectionSession {
    agent: Arc<Mutex<gewu_core::Agent>>,
    session_id: String,
    agent_name: String,
    /// Cached tool names, taken from the agent at creation. The tool set is
    /// fixed for the life of the session, so `list_tools` can be answered
    /// without locking the agent (which would block while an ask runs).
    tools: Vec<String>,
}

/// A currently running ask on a connection.
struct RunningAsk {
    request_id: String,
    handle: JoinHandle<()>,
}

/// Run the server until shutdown is requested or Ctrl+C is received.
pub async fn serve(bootstrap: Bootstrap, host: &str, ws_port: u16, tcp_port: u16) -> Result<()> {
    let ws_addr: SocketAddr = format!("{}:{}", host, ws_port)
        .parse()
        .context("Invalid WebSocket bind address")?;
    let tcp_addr: SocketAddr = format!("{}:{}", host, tcp_port)
        .parse()
        .context("Invalid TCP bind address")?;

    let ws_listener = TcpListener::bind(ws_addr)
        .await
        .with_context(|| format!("Failed to bind WebSocket listener on {}", ws_addr))?;
    let tcp_listener = TcpListener::bind(tcp_addr)
        .await
        .with_context(|| format!("Failed to bind TCP listener on {}", tcp_addr))?;

    let state = Arc::new(ServerState { bootstrap });

    info!("gewu server listening:");
    info!("  WebSocket: ws://{}", ws_addr);
    info!("  TCP (newline-delimited JSON): {}", tcp_addr);

    let mut tasks = JoinSet::new();
    tasks.spawn(accept_loop(
        ws_listener,
        state.clone(),
        Transport::WebSocket,
    ));
    tasks.spawn(accept_loop(tcp_listener, state.clone(), Transport::Tcp));

    // Run until Ctrl+C. (Programmatic shutdown is available via the
    // `shutdown_server` helper used in tests; the CLI process exits on
    // SIGINT/SIGTERM.)
    tokio::signal::ctrl_c()
        .await
        .context("Failed to listen for Ctrl+C")?;
    info!("Received Ctrl+C, shutting down");

    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}

/// The transport a connection came in on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    WebSocket,
    Tcp,
}

/// Accept connections on one listener forever.
async fn accept_loop(listener: TcpListener, state: Arc<ServerState>, transport: Transport) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    let result = match transport {
                        Transport::WebSocket => handle_ws_connection(stream, peer, state).await,
                        Transport::Tcp => handle_tcp_connection(stream, peer, state).await,
                    };
                    if let Err(e) = result {
                        warn!("Connection {} error: {}", peer, e);
                    }
                });
            }
            Err(e) => {
                warn!("Accept failed: {}", e);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

impl ServerState {
    /// Get or create the per-connection session.
    ///
    /// `session_id` and `agent_name` only apply when the session is first
    /// created; subsequent calls on the same connection reuse it.
    async fn get_or_create_session(
        &self,
        current: &mut Option<ConnectionSession>,
        session_id: Option<String>,
        agent_name: Option<String>,
    ) -> Result<ConnectionSession> {
        if let Some(existing) = current {
            return Ok(existing.clone());
        }

        let agent_name = agent_name.unwrap_or_else(|| DEFAULT_AGENT.to_string());
        if !registry::list_agent_names().contains(&agent_name) {
            anyhow::bail!("Unknown agent profile: {}", agent_name);
        }

        let session_id = session_id.unwrap_or_else(crate::bootstrap::new_session_id);
        let (agent, _registry) = create_agent(
            &agent_name,
            self.bootstrap.provider.clone(),
            self.bootstrap.pool.clone(),
            session_id.clone(),
            self.bootstrap.mcp_tools.clone(),
            self.bootstrap.log_writer.clone(),
            self.bootstrap.skills.clone(),
        )
        .await?;
        info!("Created session {} (agent: {})", session_id, agent_name);

        let agent = Arc::new(Mutex::new(agent));
        // Snapshot the tool list once; it never changes for this session.
        let tools = agent.lock().await.list_tools();

        let entry = ConnectionSession {
            agent,
            session_id,
            agent_name,
            tools,
        };
        *current = Some(entry.clone());
        Ok(entry)
    }
}

/// Append synthetic tool results for assistant tool calls that never got a
/// result (e.g. after an interrupt aborted the ask mid-tool-execution).
///
/// Without this, the next LLM request would contain an assistant message
/// with tool calls but no matching tool results, which providers reject.
async fn patch_dangling_tool_calls(agent: &mut gewu_core::Agent) {
    let messages = agent.memory.messages();
    let Some(last_assistant_index) = messages.iter().rposition(
        |m| matches!(m, Message::Assistant { tool_calls: Some(calls), .. } if !calls.is_empty()),
    ) else {
        return;
    };

    let pending_ids: Vec<(String, Option<String>)> = match &messages[last_assistant_index] {
        Message::Assistant {
            tool_calls: Some(calls),
            ..
        } => calls
            .iter()
            .map(|c| (c.id.clone(), Some(c.function.name.clone())))
            .collect(),
        _ => return,
    };

    let answered: std::collections::HashSet<String> = messages[last_assistant_index + 1..]
        .iter()
        .filter_map(|m| match m {
            Message::Tool { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect();

    let missing: Vec<(String, Option<String>)> = pending_ids
        .into_iter()
        .filter(|(id, _)| !answered.contains(id))
        .collect();
    if missing.is_empty() {
        return;
    }

    for (tool_call_id, tool_name) in missing {
        agent
            .memory
            .add_message(Message::Tool {
                tool_call_id,
                tool_name,
                content: "Interrupted by user before the tool result was recorded.".to_string(),
                media: Vec::new(),
            })
            .await;
    }
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

/// What handling a request produced.
enum Handled {
    /// A single reply message.
    Reply(ServerMessage),
    /// An ask to spawn on the connection session.
    Ask {
        ready: ServerMessage,
        request_id: String,
        prompt: String,
    },
}

/// Handle one parsed client request, producing a reply or an ask to run.
///
/// An `interrupt` request aborts and awaits the running ask task here and
/// repairs the conversation history, guaranteeing the agent lock is
/// released before any subsequent request is handled.
async fn handle_request(
    state: &ServerState,
    session: &mut Option<ConnectionSession>,
    running: &mut Option<RunningAsk>,
    request: ClientRequest,
) -> Result<Handled, RequestError> {
    match request.op {
        Op::Ping => Ok(Handled::Reply(ServerMessage::Pong { id: request.id })),

        Op::Info => Ok(Handled::Reply(ServerMessage::Info {
            id: request.id,
            model: state.bootstrap.model.clone(),
            provider: state.bootstrap.provider_name.clone(),
        })),

        Op::Usage => {
            // Measure the conversation against the agent's context window —
            // the same estimate the compaction logic acts on. With no session
            // (and none requested) there is nothing to measure; report zeros.
            if session.is_none() && request.session_id.is_none() {
                return Ok(Handled::Reply(ServerMessage::Usage {
                    id: request.id,
                    used_tokens: 0,
                    context_limit: 0,
                }));
            }
            let conn = state
                .get_or_create_session(session, request.session_id, request.agent)
                .await
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            // Never block on a running ask (the agent lock is held for the
            // whole ask): report "unknown" instead.
            let Ok(agent) = conn.agent.try_lock() else {
                return Ok(Handled::Reply(ServerMessage::Usage {
                    id: request.id,
                    used_tokens: 0,
                    context_limit: 0,
                }));
            };
            let used_tokens: usize = agent
                .memory
                .messages()
                .iter()
                .map(gewu_core::compaction::estimate_message_tokens)
                .sum();
            Ok(Handled::Reply(ServerMessage::Usage {
                id: request.id,
                used_tokens: used_tokens as u64,
                context_limit: agent.context_limit as u64,
            }))
        }

        Op::ListAgents => {
            let agents = registry::list_available_agents()
                .into_iter()
                .map(|a| a.name)
                .collect();
            Ok(Handled::Reply(ServerMessage::Agents {
                id: request.id,
                agents,
            }))
        }

        Op::ListTools => {
            let session = state
                .get_or_create_session(session, request.session_id, request.agent)
                .await
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            // Answer from the snapshot taken at session creation; this
            // avoids locking the agent, which would block while an ask is
            // running on this connection.
            Ok(Handled::Reply(ServerMessage::Tools {
                id: request.id,
                tools: session.tools,
            }))
        }

        Op::ListSessions => {
            let sessions = gewu_storage::list_sessions(&state.bootstrap.pool)
                .await
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            Ok(Handled::Reply(ServerMessage::Sessions {
                id: request.id,
                sessions,
            }))
        }

        Op::GetSession => {
            let Some(session_id) = request.session_id else {
                return Err(RequestError::Invalid(
                    "op 'get_session' requires a 'session_id' string".to_string(),
                ));
            };
            let stored = gewu_storage::load_session(&state.bootstrap.pool, &session_id)
                .await
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            Ok(Handled::Reply(ServerMessage::Session {
                id: request.id,
                session_id,
                messages: stored.into_iter().map(|stored| stored.message).collect(),
            }))
        }

        Op::DeleteSession => {
            let Some(session_id) = request.session_id else {
                return Err(RequestError::Invalid(
                    "op 'delete_session' requires a 'session_id' string".to_string(),
                ));
            };
            gewu_storage::delete_session(&state.bootstrap.pool, &session_id)
                .await
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            info!("Deleted session {}", session_id);
            Ok(Handled::Reply(ServerMessage::Deleted {
                id: request.id,
                session_id,
            }))
        }

        Op::Interrupt => {
            let request_id = running.as_ref().map(|r| r.request_id.clone());
            if let Some(running) = running.take() {
                abort_ask(running).await;
                // Repair the conversation history so the next LLM request is
                // well-formed even though the ask was cut short.
                if let Some(session) = session {
                    let mut agent = session.agent.lock().await;
                    patch_dangling_tool_calls(&mut agent).await;
                }
            }
            Ok(Handled::Reply(ServerMessage::Interrupted {
                id: Some(request.id),
                request_id,
            }))
        }

        Op::Ask { prompt } => {
            if running.is_some() {
                return Err(RequestError::Failed(
                    "an ask is already in progress; send 'interrupt' first".to_string(),
                ));
            }
            let session = state
                .get_or_create_session(session, request.session_id, request.agent)
                .await
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            let request_id = crate::bootstrap::new_session_id();
            Ok(Handled::Ask {
                ready: ServerMessage::Ready {
                    id: request.id,
                    request_id: request_id.clone(),
                    session_id: session.session_id.clone(),
                    agent: session.agent_name.clone(),
                },
                request_id,
                prompt,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Ask execution
// ---------------------------------------------------------------------------

/// Execute one ask, streaming protocol lines through `emit`.
///
/// Runs inside a spawned task while holding the agent lock. Emits `result`
/// as its final message on success, or an `error` message on failure.
async fn execute_ask<E, Fut>(
    session: &ConnectionSession,
    request_id: &str,
    prompt: &str,
    mut emit: E,
) where
    E: FnMut(ServerMessage) -> Fut,
    Fut: std::future::Future<Output = Result<(), anyhow::Error>>,
{
    let mut agent = session.agent.lock().await;
    // Repair any history left inconsistent by a previous interrupt.
    patch_dangling_tool_calls(&mut agent).await;

    let mut stream = Box::pin(agent.ask(prompt));
    // Whether a terminal frame (`result` or `error`) has been sent. Only
    // the `Finish` event and stream errors are terminal; an `AgentEvent::Error`
    // can be mid-stream (e.g. one failed tool among several).
    let mut terminal_sent = false;
    while let Some(event_result) = stream.next().await {
        let message = match event_result {
            Ok(AgentEvent::Finish { response }) => {
                terminal_sent = true;
                ServerMessage::Result {
                    id: Value::Null,
                    request_id: request_id.to_string(),
                    session_id: session.session_id.clone(),
                    response: response.clone(),
                }
            }
            Ok(event) => ServerMessage::Event {
                id: Value::Null,
                request_id: request_id.to_string(),
                event: protocol::agent_event_to_value(&event),
            },
            Err(e) => {
                terminal_sent = true;
                ServerMessage::Error {
                    id: None,
                    error: e.to_string(),
                }
            }
        };
        if emit(message).await.is_err() {
            return; // client went away; drop the stream and release the lock
        }
    }

    // The agent stream ended without a terminal event (e.g. max turns
    // reached or a doom loop produced a final `AgentEvent::Error`). Emit a
    // terminal error so the client's ask completes instead of hanging
    // forever.
    if !terminal_sent {
        let _ = emit(ServerMessage::Error {
            id: None,
            error: "ask ended without a final response".to_string(),
        })
        .await;
    }
}

/// Spawn an ask task that emits protocol lines through `out`.
///
/// All failures are reported to the client as `error` messages by
/// `execute_ask`; the task itself always completes normally.
fn spawn_ask(
    session: ConnectionSession,
    request_id: String,
    prompt: String,
    out: mpsc::Sender<String>,
    done: mpsc::UnboundedSender<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        execute_ask(&session, &request_id, &prompt, |msg| {
            let out = out.clone();
            async move { out.send(msg.to_line()).await.map_err(anyhow::Error::from) }
        })
        .await;
        let _ = done.send(());
    })
}

/// Abort a running ask and return its request id.
///
/// Awaiting the aborted handle guarantees the agent lock has been released
/// before the caller proceeds to patch memory / start a new ask.
async fn abort_ask(running: RunningAsk) -> String {
    let request_id = running.request_id;
    running.handle.abort();
    let _ = running.handle.await;
    request_id
}

// ---------------------------------------------------------------------------
// Connection loop
// ---------------------------------------------------------------------------

/// Shared request/response loop for both transports.
///
/// `incoming` yields raw client message strings; `out` serializes server
/// messages for delivery. Ask tasks run in the background and emit
/// directly into `out`, so this loop stays free to process `interrupt`.
async fn drive_connection(
    state: Arc<ServerState>,
    mut incoming: mpsc::Receiver<String>,
    out: mpsc::Sender<String>,
) -> Result<()> {
    let mut session: Option<ConnectionSession> = None;
    let mut running: Option<RunningAsk> = None;
    // Completion signals from ask tasks. The loop-side sender keeps this
    // channel open when no ask is running, so recv() simply stays pending.
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<()>();

    loop {
        tokio::select! {
            incoming = incoming.recv() => {
                let Some(raw) = incoming else { break };
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let request = match protocol::parse_request(trimmed) {
                    Ok(r) => r,
                    Err(RequestError::Invalid(msg)) | Err(RequestError::Failed(msg)) => {
                        // Echo the client's id when it can be extracted so
                        // the client can correlate the failure.
                        let id = protocol::extract_id(trimmed);
                        out.send(ServerMessage::Error { id, error: msg }.to_line())
                            .await?;
                        continue;
                    }
                };

                // Remember the correlation id so error replies can echo it.
                let request_id_value = request.id.clone();

                // An interrupt request aborts + awaits the running ask inside
                // handle_request, so the agent lock is released before any
                // further request is handled.
                match handle_request(&state, &mut session, &mut running, request).await {
                    Ok(Handled::Reply(msg)) => {
                        out.send(msg.to_line()).await?;
                    }
                    Ok(Handled::Ask { ready, request_id, prompt }) => {
                        out.send(ready.to_line()).await?;
                        let session = session
                            .clone()
                            .expect("session created by handle_request");
                        running = Some(RunningAsk {
                            request_id: request_id.clone(),
                            handle: spawn_ask(session, request_id, prompt, out.clone(), done_tx.clone()),
                        });
                    }
                    Err(RequestError::Invalid(msg)) | Err(RequestError::Failed(msg)) => {
                        out.send(
                            ServerMessage::Error { id: Some(request_id_value), error: msg }
                                .to_line(),
                        )
                        .await?;
                    }
                }
            }
            _ = done_rx.recv() => {
                // The running ask finished on its own.
                running = None;
            }
        }
    }

    if let Some(running) = running.take() {
        abort_ask(running).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Transports
// ---------------------------------------------------------------------------

/// Handle a single WebSocket connection.
async fn handle_ws_connection(
    stream: TcpStream,
    peer: SocketAddr,
    state: Arc<ServerState>,
) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(stream)
        .await
        .context("WebSocket handshake failed")?;
    info!("WebSocket client connected: {}", peer);
    let (mut sink, mut ws_stream) = ws.split();

    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    let (in_tx, in_rx) = mpsc::channel::<String>(64);

    // Writer task: drains `out_rx` into the WebSocket sink.
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if sink.send(WsMessage::text(msg)).await.is_err() {
                break;
            }
        }
    });

    // Reader task: pushes incoming text frames into `in_tx`.
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_stream.next().await {
            match msg {
                WsMessage::Text(text) => {
                    if in_tx.send(text.to_string()).await.is_err() {
                        break;
                    }
                }
                WsMessage::Close(_) => break,
                _ => {}
            }
        }
    });

    let result = drive_connection(state, in_rx, out_tx).await;

    drop(reader); // reader ends when the client disconnects
    let _ = writer.await;
    if let Err(e) = &result {
        warn!("WebSocket client {} error: {}", peer, e);
    } else {
        info!("WebSocket client disconnected: {}", peer);
    }
    result
}

/// Handle a single TCP connection (newline-delimited JSON).
async fn handle_tcp_connection(
    stream: TcpStream,
    peer: SocketAddr,
    state: Arc<ServerState>,
) -> Result<()> {
    info!("TCP client connected: {}", peer);
    let (reader_half, mut writer_half) = stream.into_split();
    let mut lines = BufReader::new(reader_half).lines();

    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    let (in_tx, in_rx) = mpsc::channel::<String>(64);

    // Writer task: drains `out_rx` into the TCP stream.
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if writer_half.write_all(msg.as_bytes()).await.is_err()
                || writer_half.write_all(b"\n").await.is_err()
                || writer_half.flush().await.is_err()
            {
                break;
            }
        }
    });

    // Reader task: pushes incoming lines into `in_tx`.
    let reader = tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            if in_tx.send(line).await.is_err() {
                break;
            }
        }
    });

    let result = drive_connection(state, in_rx, out_tx).await;

    drop(reader); // reader ends when the client disconnects
    let _ = writer.await;
    if let Err(e) = &result {
        warn!("TCP client {} error: {}", peer, e);
    } else {
        info!("TCP client disconnected: {}", peer);
    }
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_core::types::{ContentPart, ToolContext, ToolResult};
    use std::error::Error;

    // -- Test doubles -------------------------------------------------------

    struct TestLlm {
        behavior: LlmBehavior,
        /// Artificial delay before yielding events (makes asks observably
        /// long-running for interrupt tests).
        delay_ms: u64,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum LlmBehavior {
        /// Reply with text only, then finish.
        Plain,
        /// Ask for one tool call, then (second turn) finish with text.
        ToolCall,
    }

    impl TestLlm {
        fn plain() -> Arc<Self> {
            Arc::new(Self {
                behavior: LlmBehavior::Plain,
                delay_ms: 0,
            })
        }
        fn tool_call() -> Arc<Self> {
            Arc::new(Self {
                behavior: LlmBehavior::ToolCall,
                delay_ms: 0,
            })
        }
        /// Tool-calling LLM that sleeps before responding: long enough for
        /// interrupt requests to reliably arrive mid-ask.
        fn slow_tool_call() -> Arc<Self> {
            Arc::new(Self {
                behavior: LlmBehavior::ToolCall,
                delay_ms: 500,
            })
        }
    }

    #[async_trait::async_trait]
    impl gewu_core::traits::LlmProvider for TestLlm {
        fn build(
            &self,
            messages: &[Message],
            _tools: &[Arc<dyn gewu_core::traits::Tool>],
        ) -> std::result::Result<Value, Box<dyn Error + Send + Sync>> {
            Ok(serde_json::json!({ "turn": messages.len() }))
        }

        async fn send<'a>(
            &'a self,
            request: Value,
        ) -> std::result::Result<
            futures::stream::BoxStream<
                'a,
                std::result::Result<gewu_core::types::ProviderEvent, Box<dyn Error + Send + Sync>>,
            >,
            Box<dyn Error + Send + Sync>,
        > {
            let turn = request["turn"].as_u64().unwrap_or(0);
            if self.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            let events: Vec<gewu_core::types::ProviderEvent> = match self.behavior {
                LlmBehavior::Plain => vec![
                    gewu_core::types::ProviderEvent::ContentDelta {
                        delta: "Hello from test".to_string(),
                    },
                    gewu_core::types::ProviderEvent::Finish {
                        reason: Some("stop".to_string()),
                    },
                ],
                LlmBehavior::ToolCall if turn <= 1 => vec![
                    gewu_core::types::ProviderEvent::ToolCallStart {
                        id: "call-1".to_string(),
                        name: "echo".to_string(),
                    },
                    gewu_core::types::ProviderEvent::ToolCallDelta {
                        id: "call-1".to_string(),
                        delta: r#"{"value":42}"#.to_string(),
                    },
                    gewu_core::types::ProviderEvent::Finish {
                        reason: Some("tool_calls".to_string()),
                    },
                ],
                LlmBehavior::ToolCall => vec![
                    gewu_core::types::ProviderEvent::ContentDelta {
                        delta: "Tool answered".to_string(),
                    },
                    gewu_core::types::ProviderEvent::Finish {
                        reason: Some("stop".to_string()),
                    },
                ],
            };
            let s = async_stream::try_stream! {
                for event in events {
                    yield event;
                }
            };
            Ok(Box::pin(s))
        }

        async fn list_models(
            &self,
        ) -> std::result::Result<Vec<String>, Box<dyn Error + Send + Sync>> {
            Ok(Vec::new())
        }
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl gewu_core::traits::Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes its arguments"
        }
        fn schema(&self) -> schemars::schema::RootSchema {
            schemars::schema_for!(Value)
        }
        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolContext,
        ) -> std::result::Result<ToolResult, Box<dyn Error + Send + Sync>> {
            Ok(ToolResult::new("echo", format!("echoed {}", args)))
        }
    }

    fn plain_session() -> ConnectionSession {
        let agent = gewu_core::Agent::builder()
            .llm(TestLlm::plain())
            .with_tools(vec![])
            .build()
            .unwrap();
        let tools = gewu_core::ToolRegistry::list_tools(&agent);
        ConnectionSession {
            agent: Arc::new(Mutex::new(agent)),
            session_id: "sess-test".to_string(),
            agent_name: "build".to_string(),
            tools,
        }
    }

    fn tool_session() -> ConnectionSession {
        let tool: Arc<dyn gewu_core::traits::Tool> = Arc::new(EchoTool);
        let agent = gewu_core::Agent::builder()
            .llm(TestLlm::tool_call())
            .with_tools(vec![tool])
            .build()
            .unwrap();
        let tools = gewu_core::ToolRegistry::list_tools(&agent);
        ConnectionSession {
            agent: Arc::new(Mutex::new(agent)),
            session_id: "sess-test".to_string(),
            agent_name: "build".to_string(),
            tools,
        }
    }

    async fn collect_ask(session: &ConnectionSession, prompt: &str) -> Vec<String> {
        let (tx, mut rx) = mpsc::channel::<String>(256);
        let handle = tokio::spawn({
            let session = session.clone();
            let prompt = prompt.to_string();
            async move {
                execute_ask(&session, "req-test", &prompt, |msg| {
                    let tx = tx.clone();
                    async move { tx.send(msg.to_line()).await.map_err(anyhow::Error::from) }
                })
                .await;
            }
        });
        handle.await.unwrap();
        let mut lines = Vec::new();
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
        }
        lines
    }

    // -- execute_ask --------------------------------------------------------

    #[tokio::test]
    async fn ask_streams_events_then_result() {
        let session = plain_session();
        let lines = collect_ask(&session, "hi").await;

        assert!(
            lines[0].contains(r#""type":"event""#),
            "first line: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("content_delta"),
            "first line: {}",
            lines[0]
        );
        assert!(lines[0].contains(r#""request_id":"req-test""#));
        let last = lines.last().unwrap();
        assert!(last.contains(r#""type":"result""#), "last line: {}", last);
        assert!(last.contains("Hello from test"), "last line: {}", last);
    }

    #[tokio::test]
    async fn ask_with_tool_call_emits_tool_events() {
        let session = tool_session();
        let lines = collect_ask(&session, "use the tool").await;

        assert!(
            lines
                .iter()
                .any(|l| l.contains("tool_start") && l.contains("echo")),
            "expected tool_start event, got {:?}",
            lines
        );
        assert!(
            lines.iter().any(|l| l.contains("tool_finish")),
            "expected tool_finish event, got {:?}",
            lines
        );
        let last = lines.last().unwrap();
        assert!(last.contains(r#""type":"result""#), "last line: {}", last);
    }

    // -- patch_dangling_tool_calls ------------------------------------------

    #[tokio::test]
    async fn patch_appends_results_for_unanswered_tool_calls() {
        let mut agent = gewu_core::Agent::builder()
            .llm(TestLlm::plain())
            .with_tools(vec![])
            .build()
            .unwrap();

        agent
            .memory
            .add_message(Message::Assistant {
                content: None,
                tool_calls: Some(vec![gewu_core::types::ToolCall {
                    id: "call-1".to_string(),
                    r#type: "function".to_string(),
                    function: gewu_core::types::FunctionCall {
                        name: "echo".to_string(),
                        arguments: "{}".to_string(),
                    },
                }]),
            })
            .await;

        patch_dangling_tool_calls(&mut agent).await;

        let messages = agent.memory.messages();
        let last = messages.last().unwrap();
        match last {
            Message::Tool {
                tool_call_id,
                content,
                ..
            } => {
                assert_eq!(tool_call_id, "call-1");
                assert!(content.contains("Interrupted"));
            }
            other => panic!("expected Tool message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn patch_is_noop_when_tool_calls_answered() {
        let mut agent = gewu_core::Agent::builder()
            .llm(TestLlm::plain())
            .with_tools(vec![])
            .build()
            .unwrap();

        agent
            .memory
            .add_message(Message::Assistant {
                content: None,
                tool_calls: Some(vec![gewu_core::types::ToolCall {
                    id: "call-1".to_string(),
                    r#type: "function".to_string(),
                    function: gewu_core::types::FunctionCall {
                        name: "echo".to_string(),
                        arguments: "{}".to_string(),
                    },
                }]),
            })
            .await;
        agent
            .memory
            .add_message(Message::Tool {
                tool_call_id: "call-1".to_string(),
                tool_name: Some("echo".to_string()),
                content: "done".to_string(),
                media: Vec::new(),
            })
            .await;

        let before = agent.memory.messages().len();
        patch_dangling_tool_calls(&mut agent).await;
        assert_eq!(agent.memory.messages().len(), before);
    }

    #[tokio::test]
    async fn patch_is_noop_without_assistant_tool_calls() {
        let mut agent = gewu_core::Agent::builder()
            .llm(TestLlm::plain())
            .with_tools(vec![])
            .build()
            .unwrap();

        agent
            .memory
            .add_message(Message::User {
                content: vec![ContentPart::Text {
                    text: "hi".to_string(),
                }],
            })
            .await;

        let before = agent.memory.messages().len();
        patch_dangling_tool_calls(&mut agent).await;
        assert_eq!(agent.memory.messages().len(), before);
    }

    // -- drive_connection ---------------------------------------------------

    /// Starts a `drive_connection` loop with plain test wiring and returns
    /// the request/out channel handles for driving it from the test.
    async fn spawn_connection(
        state: Arc<ServerState>,
    ) -> (
        mpsc::Sender<String>,
        mpsc::Receiver<String>,
        JoinHandle<Result<()>>,
    ) {
        let (in_tx, in_rx) = mpsc::channel::<String>(16);
        let (out_tx, out_rx) = mpsc::channel::<String>(256);
        let handle = tokio::spawn(drive_connection(state, in_rx, out_tx));
        (in_tx, out_rx, handle)
    }

    async fn recv_json(out_rx: &mut mpsc::Receiver<String>) -> Value {
        let line = out_rx.recv().await.expect("server message");
        serde_json::from_str(&line).expect("valid JSON line")
    }

    fn server_state_with(provider: Arc<dyn gewu_core::traits::LlmProvider>) -> Arc<ServerState> {
        Arc::new(ServerState {
            bootstrap: Bootstrap {
                provider,
                pool: sqlx::sqlite::SqlitePoolOptions::new()
                    .connect_lazy("sqlite::memory:")
                    .unwrap(),
                mcp_tools: Vec::new(),
                skills: Vec::new(),
                log_writer: None,
                model: "test-model".to_string(),
                provider_name: "test".to_string(),
            },
        })
    }

    #[tokio::test]
    async fn connection_replies_to_ping_and_list_agents() {
        let state = server_state_with(TestLlm::plain());
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        in_tx
            .send(r#"{"id": 1, "op": "ping"}"#.to_string())
            .await
            .unwrap();
        let pong = recv_json(&mut out_rx).await;
        assert_eq!(pong["type"], "pong");
        assert_eq!(pong["id"], 1);

        in_tx
            .send(r#"{"id": 2, "op": "list_agents"}"#.to_string())
            .await
            .unwrap();
        let agents = recv_json(&mut out_rx).await;
        assert_eq!(agents["type"], "agents");
        assert!(agents["agents"].as_array().unwrap().len() >= 3);

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn connection_reports_context_usage() {
        let state = server_state_with(TestLlm::plain());
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        // No session yet: nothing to measure.
        in_tx
            .send(r#"{"id": 1, "op": "usage"}"#.to_string())
            .await
            .unwrap();
        let usage = recv_json(&mut out_rx).await;
        assert_eq!(usage["type"], "usage");
        assert_eq!(usage["used_tokens"], 0);
        assert_eq!(usage["context_limit"], 0);

        // After an ask the conversation has measurable content.
        in_tx
            .send(r#"{"id": 2, "op": "ask", "prompt": "hello"}"#.to_string())
            .await
            .unwrap();
        let _ready = recv_json(&mut out_rx).await;
        let _event = recv_json(&mut out_rx).await;
        let _result = recv_json(&mut out_rx).await;

        in_tx
            .send(r#"{"id": 3, "op": "usage"}"#.to_string())
            .await
            .unwrap();
        let usage = recv_json(&mut out_rx).await;
        assert_eq!(usage["type"], "usage");
        assert!(
            usage["used_tokens"].as_u64().unwrap() > 0,
            "ask history should count: {usage}"
        );
        assert_eq!(usage["context_limit"], 128_000);

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn connection_reports_model_and_provider() {
        let state = server_state_with(TestLlm::plain());
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        in_tx
            .send(r#"{"id": 1, "op": "info"}"#.to_string())
            .await
            .unwrap();
        let info = recv_json(&mut out_rx).await;
        assert_eq!(info["type"], "info");
        assert_eq!(info["id"], 1);
        assert_eq!(info["model"], "test-model");
        assert_eq!(info["provider"], "test");

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn connection_lists_and_loads_sessions() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE memory_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        let message = Message::User {
            content: vec![ContentPart::Text {
                text: "hi".to_string(),
            }],
        };
        sqlx::query(
            "INSERT INTO memory_messages (session_id, content, created_at) VALUES (?, ?, ?)",
        )
        .bind("s1")
        .bind(serde_json::to_string(&message).unwrap())
        .bind(1)
        .execute(&pool)
        .await
        .unwrap();

        let state = Arc::new(ServerState {
            bootstrap: Bootstrap {
                provider: TestLlm::plain(),
                pool,
                mcp_tools: Vec::new(),
                skills: Vec::new(),
                log_writer: None,
                model: "test-model".to_string(),
                provider_name: "test".to_string(),
            },
        });
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        in_tx
            .send(r#"{"id": 1, "op": "list_sessions"}"#.to_string())
            .await
            .unwrap();
        let sessions = recv_json(&mut out_rx).await;
        assert_eq!(sessions["type"], "sessions");
        assert_eq!(sessions["sessions"][0]["session_id"], "s1");
        assert_eq!(sessions["sessions"][0]["message_count"], 1);

        in_tx
            .send(r#"{"id": 2, "op": "get_session", "session_id": "s1"}"#.to_string())
            .await
            .unwrap();
        let session = recv_json(&mut out_rx).await;
        assert_eq!(session["type"], "session");
        assert_eq!(session["session_id"], "s1");
        assert_eq!(session["messages"].as_array().unwrap().len(), 1);

        in_tx
            .send(r#"{"id": 3, "op": "get_session", "session_id": "missing"}"#.to_string())
            .await
            .unwrap();
        let empty = recv_json(&mut out_rx).await;
        assert_eq!(empty["type"], "session");
        assert_eq!(empty["messages"].as_array().unwrap().len(), 0);

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn connection_rejects_bad_requests() {
        let state = server_state_with(TestLlm::plain());
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        in_tx.send("not json".to_string()).await.unwrap();
        let err = recv_json(&mut out_rx).await;
        assert_eq!(err["type"], "error");
        assert!(err["error"].as_str().unwrap().contains("JSON"));

        in_tx
            .send(r#"{"id": 1, "op": "unknown"}"#.to_string())
            .await
            .unwrap();
        let err = recv_json(&mut out_rx).await;
        assert_eq!(err["type"], "error");
        assert!(err["error"].as_str().unwrap().contains("unknown op"));

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn connection_runs_ask_and_streams_events() {
        let state = server_state_with(TestLlm::plain());
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        in_tx
            .send(r#"{"id": 1, "op": "ask", "prompt": "hello"}"#.to_string())
            .await
            .unwrap();

        let ready = recv_json(&mut out_rx).await;
        assert_eq!(ready["type"], "ready");
        assert_eq!(ready["id"], 1);
        let request_id = ready["request_id"].as_str().unwrap().to_string();
        let session_id = ready["session_id"].as_str().unwrap().to_string();
        assert!(!request_id.is_empty());
        assert!(!session_id.is_empty());

        // Events then result.
        let event = recv_json(&mut out_rx).await;
        assert_eq!(event["type"], "event");
        assert_eq!(event["event"]["type"], "content_delta");
        let result = recv_json(&mut out_rx).await;
        assert_eq!(result["type"], "result");
        assert_eq!(result["response"], "Hello from test");
        assert_eq!(result["request_id"], request_id.as_str());

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn connection_deletes_a_session_from_the_store() {
        let state = server_state_with(TestLlm::plain());
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        // Produce a stored session.
        in_tx
            .send(r#"{"id": 1, "op": "ask", "prompt": "hello"}"#.to_string())
            .await
            .unwrap();
        let _ready = recv_json(&mut out_rx).await;
        let _event = recv_json(&mut out_rx).await;
        let result = recv_json(&mut out_rx).await;
        assert_eq!(result["type"], "result");
        let session_id = result["session_id"].as_str().unwrap().to_string();

        in_tx
            .send(r#"{"id": 2, "op": "list_sessions"}"#.to_string())
            .await
            .unwrap();
        let sessions = recv_json(&mut out_rx).await;
        assert_eq!(sessions["sessions"].as_array().unwrap().len(), 1);

        in_tx
            .send(
                format!(r#"{{"id": 3, "op": "delete_session", "session_id": "{session_id}"}}"#)
                    .to_string(),
            )
            .await
            .unwrap();
        let deleted = recv_json(&mut out_rx).await;
        assert_eq!(deleted["type"], "deleted");
        assert_eq!(deleted["session_id"], session_id.as_str());

        in_tx
            .send(r#"{"id": 4, "op": "list_sessions"}"#.to_string())
            .await
            .unwrap();
        let sessions = recv_json(&mut out_rx).await;
        assert_eq!(sessions["sessions"].as_array().unwrap().len(), 0);

        in_tx
            .send(
                format!(r#"{{"id": 5, "op": "get_session", "session_id": "{session_id}"}}"#)
                    .to_string(),
            )
            .await
            .unwrap();
        let empty = recv_json(&mut out_rx).await;
        assert_eq!(empty["messages"].as_array().unwrap().len(), 0);

        // Malformed request: missing session id.
        in_tx
            .send(r#"{"id": 6, "op": "delete_session"}"#.to_string())
            .await
            .unwrap();
        let err = recv_json(&mut out_rx).await;
        assert_eq!(err["type"], "error");
        assert!(err["error"].as_str().unwrap().contains("session_id"));

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn connection_rejects_second_ask_while_running() {
        // A tool-calling agent keeps the ask busy across two LLM turns.
        let state = server_state_with(TestLlm::tool_call());
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        in_tx
            .send(r#"{"id": 1, "op": "ask", "prompt": "go"}"#.to_string())
            .await
            .unwrap();
        let ready = recv_json(&mut out_rx).await;
        assert_eq!(ready["type"], "ready");

        // Wait until the first event arrives so the ask is definitely running.
        let event = recv_json(&mut out_rx).await;
        assert_eq!(event["type"], "event");

        in_tx
            .send(r#"{"id": 2, "op": "ask", "prompt": "again"}"#.to_string())
            .await
            .unwrap();
        let err = recv_json(&mut out_rx).await;
        assert_eq!(err["type"], "error");
        assert!(
            err["error"]
                .as_str()
                .unwrap()
                .contains("already in progress")
        );

        // Let the first ask finish.
        let mut saw_result = false;
        for _ in 0..10 {
            let msg = recv_json(&mut out_rx).await;
            if msg["type"] == "result" {
                saw_result = true;
                break;
            }
        }
        assert!(saw_result, "first ask should complete");

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn connection_interrrupt_aborts_running_ask_and_repairs_history() {
        let state = server_state_with(TestLlm::slow_tool_call());
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        in_tx
            .send(r#"{"id": 1, "op": "ask", "prompt": "go"}"#.to_string())
            .await
            .unwrap();
        let ready = recv_json(&mut out_rx).await;
        assert_eq!(ready["type"], "ready");
        let request_id = ready["request_id"].as_str().unwrap().to_string();

        // The ask is sleeping in the LLM call; interrupt it now.
        in_tx
            .send(r#"{"id": 2, "op": "interrupt"}"#.to_string())
            .await
            .unwrap();
        let interrupted = recv_json(&mut out_rx).await;
        assert_eq!(interrupted["type"], "interrupted");
        assert_eq!(interrupted["request_id"], request_id.as_str());
        assert_eq!(interrupted["id"], 2);

        // The loop should now accept another request (the aborted one
        // released the agent).
        in_tx
            .send(r#"{"id": 3, "op": "ping"}"#.to_string())
            .await
            .unwrap();
        let pong = recv_json(&mut out_rx).await;
        assert_eq!(pong["type"], "pong");

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn error_replies_echo_the_client_id() {
        let state = server_state_with(TestLlm::plain());
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        // A known op that fails server-side echoes the parsed request id.
        in_tx
            .send(r#"{"id": "abc", "op": "ask", "prompt": "go", "agent": "nope"}"#.to_string())
            .await
            .unwrap();
        let err = recv_json(&mut out_rx).await;
        assert_eq!(err["type"], "error");
        assert_eq!(err["id"], "abc");

        // A malformed message echoes the raw id field when one is present.
        in_tx
            .send(r#"{"id": 42, "op": "bogus"}"#.to_string())
            .await
            .unwrap();
        let err = recv_json(&mut out_rx).await;
        assert_eq!(err["type"], "error");
        assert_eq!(err["id"], 42);

        // No id present at all stays null.
        in_tx.send("not json".to_string()).await.unwrap();
        let err = recv_json(&mut out_rx).await;
        assert_eq!(err["type"], "error");
        assert!(err.get("id").is_none() || err["id"].is_null());

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    /// LLM that always asks for the `echo` tool, forever.
    struct ToolCallLoopLlm;

    #[async_trait::async_trait]
    impl gewu_core::traits::LlmProvider for ToolCallLoopLlm {
        fn build(
            &self,
            _messages: &[Message],
            _tools: &[Arc<dyn gewu_core::traits::Tool>],
        ) -> std::result::Result<Value, Box<dyn Error + Send + Sync>> {
            Ok(serde_json::json!({}))
        }

        async fn send<'a>(
            &'a self,
            _request: Value,
        ) -> std::result::Result<
            futures::stream::BoxStream<
                'a,
                std::result::Result<gewu_core::types::ProviderEvent, Box<dyn Error + Send + Sync>>,
            >,
            Box<dyn Error + Send + Sync>,
        > {
            let s = async_stream::try_stream! {
                yield gewu_core::types::ProviderEvent::ToolCallStart {
                    id: "call-1".to_string(),
                    name: "echo".to_string(),
                };
                yield gewu_core::types::ProviderEvent::ToolCallDelta {
                    id: "call-1".to_string(),
                    delta: "{}".to_string(),
                };
                yield gewu_core::types::ProviderEvent::Finish {
                    reason: Some("tool_calls".to_string()),
                };
            };
            Ok(Box::pin(s))
        }

        async fn list_models(
            &self,
        ) -> std::result::Result<Vec<String>, Box<dyn Error + Send + Sync>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn ask_ending_without_finish_emits_terminal_error() {
        // max_turns(1) forces the agent to end with a final AgentEvent::Error
        // after the first tool turn, never reaching a Finish.
        let agent = gewu_core::Agent::builder()
            .llm(Arc::new(ToolCallLoopLlm))
            .with_tools(vec![Arc::new(EchoTool) as Arc<dyn gewu_core::traits::Tool>])
            .max_turns(Some(1))
            .build()
            .unwrap();
        let session = ConnectionSession {
            agent: Arc::new(Mutex::new(agent)),
            session_id: "sess-test".to_string(),
            agent_name: "build".to_string(),
            tools: vec!["echo".to_string()],
        };

        let (tx, mut rx) = mpsc::channel::<String>(256);
        let handle = tokio::spawn({
            let session = session.clone();
            async move {
                execute_ask(&session, "req-test", "go", |msg| {
                    let tx = tx.clone();
                    async move { tx.send(msg.to_line()).await.map_err(anyhow::Error::from) }
                })
                .await;
            }
        });
        handle.await.unwrap();

        let mut saw_error_event = false;
        let mut terminal = None;
        while let Ok(line) = rx.try_recv() {
            let msg: Value = serde_json::from_str(&line).unwrap();
            match msg["type"].as_str() {
                Some("event") if msg["event"]["type"] == "error" => saw_error_event = true,
                Some("error") => terminal = Some(msg),
                _ => {}
            }
        }
        assert!(
            saw_error_event,
            "expected a streamed error event before the terminal frame"
        );
        let terminal = terminal.expect("terminal error frame");
        assert!(
            terminal["error"]
                .as_str()
                .unwrap()
                .contains("without a final response"),
            "terminal frame: {}",
            terminal
        );
    }

    #[tokio::test]
    async fn list_tools_answers_from_snapshot_while_ask_runs() {
        let state = server_state_with(TestLlm::tool_call());
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        // The first ask creates the connection session (explore profile has
        // a fixed, non-empty tool set).
        in_tx
            .send(r#"{"id": 1, "op": "ask", "prompt": "go", "agent": "explore"}"#.to_string())
            .await
            .unwrap();
        let ready = recv_json(&mut out_rx).await;
        assert_eq!(ready["type"], "ready");

        // Ask is now running; list_tools must still answer promptly from
        // the session snapshot instead of blocking on the agent lock.
        in_tx
            .send(r#"{"id": 2, "op": "list_tools"}"#.to_string())
            .await
            .unwrap();
        let tools = recv_json(&mut out_rx).await;
        assert_eq!(tools["type"], "tools");
        assert_eq!(tools["id"], 2);
        assert!(
            tools["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t.as_str() == Some("read_file")),
            "expected read_file in {:?}",
            tools["tools"]
        );

        // Drain to the result so the ask task completes cleanly.
        let mut saw_result = false;
        for _ in 0..10 {
            let msg = recv_json(&mut out_rx).await;
            if msg["type"] == "result" {
                saw_result = true;
                break;
            }
        }
        assert!(saw_result, "first ask should complete");

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }
}
