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
//! previous CLI run) to resume an existing conversation. A later request
//! naming a *different* `session_id`/`agent` is rejected instead of silently
//! ignored.
//!
//! One `ask` runs at a time per connection — and per session across
//! connections: a session busy with an ask on one connection rejects asks
//! from other connections. While an ask is running the connection can still
//! send `interrupt`, which aborts the running task, repairs the
//! conversation history (dropping the unanswered prompt), and keeps it
//! consistent.
//!
//! When started with a token, every connection must send an `auth` request
//! before any other operation. TCP lines longer than [`MAX_LINE_BYTES`] and
//! lines that are not valid UTF-8 are rejected with an error frame; the
//! connection stays alive.

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use gewu_core::ToolRegistry;
use gewu_core::types::{AgentEvent, Message, PathEnforcement};
pub use gewu_protocol as protocol;
use gewu_protocol::FileEntry;
use registry::create_agent;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
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

/// Maximum accepted inbound TCP line length. Larger lines (and lines that
/// never terminate within this budget) are rejected with an error frame
/// instead of being buffered without bound.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Shared server context handed to each connection handler.
struct ServerState {
    bootstrap: Bootstrap,
    /// When set, connections must authenticate with an `auth` request
    /// carrying this token before any other operation.
    token: Option<String>,
    /// Session ids with an ask currently running, across all connections.
    /// Guards one conversation from concurrent writes by two connections.
    busy_sessions: Mutex<HashSet<String>>,
    sessions: Mutex<HashMap<String, ConnectionSession>>,
    turns: Mutex<HashMap<String, Arc<Mutex<LiveTurn>>>>,
    /// Serializes turn registration, snapshots, and lifecycle mutations.
    lifecycle: Mutex<()>,
}

#[derive(Clone)]
struct Subscriber {
    out: mpsc::Sender<String>,
    failed: tokio::sync::watch::Sender<bool>,
    persistent: bool,
}

impl Subscriber {
    fn send(&self, line: &str) -> bool {
        if self.out.try_send(line.to_owned()).is_err() {
            self.failed.send_replace(true);
            false
        } else {
            true
        }
    }
}

/// Session-owned execution and reconnect state. Socket writers never own
/// the worker and never backpressure it.
struct LiveTurn {
    request_id: String,
    messages: Vec<Message>,
    frames: Vec<ServerMessage>,
    bytes: usize,
    seq: u64,
    busy: bool,
    /// The model selector the turn runs on, echoed in reconnect snapshots.
    model: String,
    /// Notified (waiters) when `busy` flips back to false at turn
    /// finalization, so interrupts await completion without spinning.
    done: Arc<tokio::sync::Notify>,
    subscribers: Vec<Subscriber>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    terminal: Option<ServerMessage>,
}

impl LiveTurn {
    fn publish(&mut self, message: ServerMessage) {
        self.seq += 1;
        let line = message.to_line();
        self.bytes += line.len();
        self.frames.push(message);
        if self.bytes > 16 * 1024 * 1024
            && let Some(cancel) = self.cancel.take()
        {
            let _ = cancel.send(());
        }
        // Disconnect slow readers rather than silently losing updates.
        self.subscribers.retain(|subscriber| subscriber.send(&line));
    }
}

async fn attach_subscriber(
    state: &ServerState,
    session_id: &str,
    subscriber: Subscriber,
) -> Result<(), RequestError> {
    let turn = state.turns.lock().await.get(session_id).cloned();
    if let Some(turn) = turn {
        let mut live = turn.lock().await;
        // Hold the turn lock through snapshot send and registration; a
        // publisher cannot send an event into the snapshot/live gap.
        let snapshot = ServerMessage::SessionSnapshot {
            session_id: session_id.to_owned(),
            messages: live.messages.clone(),
            frames: live.frames.clone(),
            busy: live.busy,
            seq: live.seq,
            model: Some(live.model.clone()),
        };
        subscriber
            .out
            .send(snapshot.to_line())
            .await
            .map_err(|_| RequestError::Failed("subscriber disconnected".into()))?;
        live.subscribers
            .retain(|s| !s.out.same_channel(&subscriber.out));
        live.subscribers.push(subscriber);
        return Ok(());
    }
    let Some(session) = state.sessions.lock().await.get(session_id).cloned() else {
        return Err(RequestError::Failed("unknown session".into()));
    };
    let messages = session.agent.lock().await.memory.messages().to_vec();
    subscriber
        .out
        .send(
            ServerMessage::SessionSnapshot {
                session_id: session_id.to_owned(),
                messages: messages.clone(),
                frames: vec![],
                busy: false,
                seq: 0,
                model: Some(session.model.clone()),
            }
            .to_line(),
        )
        .await
        .map_err(|_| RequestError::Failed("subscriber disconnected".into()))?;
    state.turns.lock().await.insert(
        session_id.to_owned(),
        Arc::new(Mutex::new(LiveTurn {
            request_id: String::new(),
            messages,
            frames: vec![],
            bytes: 0,
            seq: 0,
            busy: false,
            model: session.model.clone(),
            done: Arc::new(tokio::sync::Notify::new()),
            subscribers: vec![subscriber],
            cancel: None,
            terminal: None,
        })),
    );
    Ok(())
}

/// Per-connection session: one agent plus its identity.
#[derive(Clone)]
struct ConnectionSession {
    agent: Arc<Mutex<gewu_core::Agent>>,
    session_id: String,
    agent_name: String,
    /// The session's model selector (`endpoint/model`) into the bootstrap
    /// catalog; switchable at runtime via `switch_model`.
    model: String,
    /// The session's provider (the catalog entry for `model`).
    provider: Arc<dyn gewu_core::traits::LlmProvider>,
    /// Resolved context window (tokens) of the session's model.
    context_limit: usize,
    /// Cached tool names, taken from the agent at creation. The tool set is
    /// fixed for the life of the session, so `list_tools` can be answered
    /// without locking the agent (which would block while an ask runs).
    tools: Vec<String>,
    /// The running turn's steering handle, published by `execute_ask` while
    /// the agent lock is held so `steer` requests can inject messages
    /// without touching the agent. `None` while idle.
    steering: Arc<Mutex<Option<gewu_core::Steering>>>,
}

/// A currently running ask on a connection.
struct RunningAsk {
    session_id: String,
}

/// Run the server until shutdown is requested or Ctrl+C is received.
pub async fn serve(
    bootstrap: Bootstrap,
    host: &str,
    ws_port: u16,
    tcp_port: u16,
    token: Option<String>,
) -> Result<()> {
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

    let state = Arc::new(ServerState {
        bootstrap,
        token,
        busy_sessions: Mutex::new(HashSet::new()),
        sessions: Mutex::new(HashMap::new()),
        turns: Mutex::new(HashMap::new()),
        lifecycle: Mutex::new(()),
    });

    info!("gewu server listening:");
    info!("  WebSocket: ws://{}", ws_addr);
    info!("  TCP (newline-delimited JSON): {}", tcp_addr);
    if state.token.is_some() {
        info!("authentication required (clients send the 'auth' op first)");
    }

    // Per-connection handlers are spawned into a shared tracking set so
    // shutdown can abort and drain them together with the listeners.
    let mut connections: JoinSet<()> = JoinSet::new();
    let (conn_tx, mut conn_rx) = mpsc::unbounded_channel::<JoinHandle<()>>();

    let mut tasks = JoinSet::new();
    tasks.spawn(accept_loop(
        ws_listener,
        state.clone(),
        Transport::WebSocket,
        conn_tx.clone(),
    ));
    tasks.spawn(accept_loop(
        tcp_listener,
        state.clone(),
        Transport::Tcp,
        conn_tx.clone(),
    ));
    drop(conn_tx); // accept loops hold the remaining senders

    // Run until Ctrl+C (or until both accept loops exit). Connection
    // handles arriving from the accept loops are moved into the tracking
    // set as they are created.
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        tokio::select! {
            result = &mut ctrl_c => {
                result.context("Failed to listen for Ctrl+C")?;
                info!("Received Ctrl+C, shutting down");
                break;
            }
            handle = conn_rx.recv() => {
                match handle {
                    Some(handle) => {
                        connections.spawn(async move {
                            let _ = handle.await;
                        });
                    }
                    // Both accept loops are gone; nothing left to serve.
                    None => break,
                }
            }
        }
    }

    // Abort listeners first (no new connections), then abort and drain every
    // tracked connection task so nothing outlives the shutdown.
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    state.shutdown_turns().await;
    Ok(())
}

/// The transport a connection came in on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    WebSocket,
    Tcp,
}

/// Accept connections on one listener forever, forwarding each connection
/// task's handle to the tracker.
async fn accept_loop(
    listener: TcpListener,
    state: Arc<ServerState>,
    transport: Transport,
    conn_tx: mpsc::UnboundedSender<JoinHandle<()>>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let state = state.clone();
                let handle = tokio::spawn(async move {
                    let result = match transport {
                        Transport::WebSocket => handle_ws_connection(stream, peer, state).await,
                        Transport::Tcp => handle_tcp_connection(stream, peer, state).await,
                    };
                    if let Err(e) = result {
                        warn!("Connection {} error: {}", peer, e);
                    }
                });
                let _ = conn_tx.send(handle);
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
    /// created; subsequent calls on the same connection reuse it. A later
    /// request naming a *different* `session_id` or `agent` is rejected so
    /// mismatches surface as errors instead of being silently ignored.
    async fn get_or_create_session(
        &self,
        current: &mut Option<ConnectionSession>,
        session_id: Option<String>,
        agent_name: Option<String>,
    ) -> Result<ConnectionSession> {
        if let Some(existing) = current {
            if let Some(requested) = &session_id
                && requested != &existing.session_id
            {
                anyhow::bail!(
                    "session mismatch: this connection is bound to session '{}'; \
                     requested '{}'. Open a new connection to use another session.",
                    existing.session_id,
                    requested
                );
            }
            if let Some(requested) = &agent_name
                && requested != &existing.agent_name
            {
                anyhow::bail!(
                    "agent mismatch: this connection is bound to agent '{}'; \
                     requested '{}'. Open a new connection to use another agent.",
                    existing.agent_name,
                    requested
                );
            }
            return Ok(existing.clone());
        }

        let session_id = session_id.unwrap_or_else(crate::bootstrap::new_session_id);
        // Serialize initialization so multiple observers share one agent.
        let mut sessions = self.sessions.lock().await;
        if let Some(existing) = sessions.get(&session_id) {
            if agent_name
                .as_ref()
                .is_some_and(|name| name != &existing.agent_name)
            {
                anyhow::bail!("session agent mismatch");
            }
            *current = Some(existing.clone());
            return Ok(existing.clone());
        }
        let agent_name = match agent_name {
            Some(name) => name,
            None => gewu_storage::session_agent(&self.bootstrap.pool, &session_id).await?,
        };
        if !registry::list_agent_names().contains(&agent_name) {
            anyhow::bail!("Unknown agent profile: {}", agent_name);
        }

        // Resolve the session's model: the persisted selector when it still
        // exists in the catalog, else the server default.
        let model = match gewu_storage::session_model(&self.bootstrap.pool, &session_id).await? {
            Some(stored) if self.bootstrap.models.contains_key(&stored) => stored,
            Some(stored) => {
                warn!(
                    "session {} stored unknown model '{}'; using {}",
                    session_id, stored, self.bootstrap.model_selector
                );
                self.bootstrap.model_selector.clone()
            }
            None => self.bootstrap.model_selector.clone(),
        };
        let entry = self
            .bootstrap
            .models
            .get(&model)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("model '{model}' is not in the catalog"))?;

        let (agent, _registry) = create_agent(
            registry::ProfileInputs {
                model_catalog: Some(registry::ModelCatalog {
                    entries: Arc::new(self.bootstrap.models.clone()),
                    default: self.bootstrap.model_selector.clone(),
                }),
                name: &agent_name,
                provider: entry.provider.clone(),
                pool: self.bootstrap.pool.clone(),
                working_directory: self.bootstrap.working_directory.clone().into(),
                additional_tools: self.bootstrap.mcp_tools.clone(),
                skills: self.bootstrap.skills.clone(),
                web_fetch: self.bootstrap.web_fetch.clone(),
                enforcement: self.bootstrap.enforcement,
                context_limit: Some(entry.context_limit),
                agents: self.bootstrap.agents.clone(),
                session_id: &session_id,
            },
            self.bootstrap.log_writer.clone(),
        )
        .await?;
        info!(
            "Created session {} (agent: {}, model: {})",
            session_id, agent_name, model
        );

        let steering = agent.steering().clone();
        let agent = Arc::new(Mutex::new(agent));
        {
            let mut agent = agent.lock().await;
            patch_dangling_tool_calls(&mut agent).await;
        }
        gewu_storage::set_session_agent(&self.bootstrap.pool, &session_id, &agent_name).await?;
        // Snapshot the tool list once; it never changes for this session.
        let tools = agent.lock().await.list_tools();

        let entry = ConnectionSession {
            agent,
            session_id,
            agent_name,
            model,
            provider: entry.provider,
            context_limit: entry.context_limit,
            tools,
            steering: Arc::new(Mutex::new(Some(steering))),
        };
        *current = Some(entry.clone());
        sessions.insert(entry.session_id.clone(), entry.clone());
        Ok(entry)
    }

    /// Claim a session for an ask. Returns `false` when another connection
    /// is already running an ask on that session.
    async fn claim_session(&self, session_id: &str) -> bool {
        self.busy_sessions
            .lock()
            .await
            .insert(session_id.to_string())
    }

    /// Release a session claim after its ask finished, failed, or was
    /// interrupted. Idempotent.
    async fn release_session(&self, session_id: &str) {
        self.busy_sessions.lock().await.remove(session_id);
    }

    async fn cancel_turn(&self, session_id: &str) -> Option<String> {
        if let Some(session) = self.sessions.lock().await.get(session_id)
            && let Some(inbox) = session.steering.lock().await.as_ref()
        {
            inbox.pause();
        }
        let turn = self.turns.lock().await.get(session_id).cloned()?;
        let request_id = {
            let mut turn = turn.lock().await;
            if !turn.busy {
                return None;
            }
            if let Some(cancel) = turn.cancel.take() {
                let _ = cancel.send(());
            }
            turn.request_id.clone()
        };
        // Finalization repairs history before releasing the session claim.
        // Await the finalizer's notification instead of spinning: this runs
        // while the global lifecycle lock is held, so a busy-wait would stall
        // every other connection. The `Notify` is registered before `busy` is
        // re-checked so a concurrent finalization cannot be missed.
        let done = turn.lock().await.done.clone();
        loop {
            let notified = done.notified();
            tokio::pin!(notified);
            if !turn.lock().await.busy {
                break;
            }
            notified.await;
        }
        Some(request_id)
    }

    async fn shutdown_turns(&self) {
        let ids: Vec<_> = self.turns.lock().await.keys().cloned().collect();
        for id in ids {
            self.cancel_turn(&id).await;
        }
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

/// Drop the user prompt of an aborted ask when it produced no output.
///
/// If an ask is interrupted before the model responded, the only trace of it
/// is the trailing user message the ask appended; leaving it in the history
/// would steer the next ask towards the cancelled instruction. When the
/// aborted exchange already produced assistant/tool output, the history is
/// kept (and only repaired by [`patch_dangling_tool_calls`]) so executed
/// work is not lost.
async fn drop_dangling_prompt(agent: &mut gewu_core::Agent) {
    if matches!(agent.memory.messages().last(), Some(Message::User { .. })) {
        let mut kept = agent.memory.messages().to_vec();
        kept.pop();
        agent.memory.replace(kept).await;
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
        session_id: String,
        prompt: String,
    },
    Subscribe {
        id: Value,
        session: ConnectionSession,
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
    if let Some(active) = running.as_ref()
        && !state
            .busy_sessions
            .lock()
            .await
            .contains(&active.session_id)
    {
        *running = None;
    }
    match request.op {
        Op::SubscribeSession => {
            let current = state
                .get_or_create_session(session, request.session_id, request.agent)
                .await
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            Ok(Handled::Subscribe {
                id: request.id,
                session: current,
            })
        }
        Op::RevertChat { turn, undo } => {
            if running.is_some() {
                return Err(RequestError::Failed("cannot revert a running turn".into()));
            }
            let current = state
                .get_or_create_session(session, request.session_id, None)
                .await
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            if !state.claim_session(&current.session_id).await {
                return Err(RequestError::Failed("session is busy".into()));
            }
            let result = async {
                let reminder = current.agent.lock().await.reminder().clone();
                let (messages, restored_reminder) = gewu_storage::revert_chat(
                    &state.bootstrap.pool,
                    &current.session_id,
                    turn,
                    undo,
                    Some(&reminder),
                )
                .await?;
                let memory = gewu_storage::SqliteMemory::new(
                    state.bootstrap.pool.clone(),
                    current.session_id.clone(),
                )
                .await?;
                {
                    let mut agent = current.agent.lock().await;
                    agent.memory = Box::new(memory);
                    let next = match restored_reminder {
                        // Undo restores the exact state snapshotted with the messages.
                        Some(restored) => restored,
                        // Forward revert: if the retained history lost its
                        // reminder (the boundary cut the message carrying it),
                        // re-arm so the profile never becomes unenforced.
                        None => {
                            let (full, sparse) = agent.reminder().texts();
                            if gewu_core::agent::Reminder::in_history(&messages) {
                                let user_turns = messages
                                    .iter()
                                    .filter(|m| matches!(m, Message::User { .. }))
                                    .count();
                                gewu_core::agent::Reminder::continue_at(full, sparse, user_turns)
                            } else {
                                registry::profile_reminder(None, &current.agent_name)
                            }
                        }
                    };
                    agent.set_reminder(next);
                    if let Err(e) = gewu_storage::set_reminder(
                        &state.bootstrap.pool,
                        &current.session_id,
                        agent.reminder(),
                    )
                    .await
                    {
                        tracing::error!("failed to persist reminder state after revert: {e}");
                    }
                }
                Ok::<_, anyhow::Error>(messages)
            }
            .await;
            state.release_session(&current.session_id).await;
            if let Ok(messages) = &result {
                // Keep the idle turn entry alive: observers registered on it
                // must see the reverted history and stay attached, so the
                // next turn's subscriber handoff finds them. Removing the
                // entry silently detached every observer.
                if let Some(turn) = state.turns.lock().await.get(&current.session_id).cloned() {
                    let mut live = turn.lock().await;
                    live.messages = messages.clone();
                    let seq = live.seq + 1;
                    let model = live.model.clone();
                    live.publish(ServerMessage::SessionSnapshot {
                        session_id: current.session_id.clone(),
                        messages: messages.clone(),
                        frames: vec![],
                        busy: false,
                        seq,
                        model: Some(model),
                    });
                }
            }
            result
                .map(|messages| {
                    Handled::Reply(ServerMessage::Session {
                        id: request.id,
                        session_id: current.session_id.clone(),
                        messages,
                    })
                })
                .map_err(|e| RequestError::Failed(e.to_string()))
        }
        Op::SwitchAgent { agent: target } => {
            if running.is_some() {
                return Err(RequestError::Failed(
                    "cannot switch agent while a turn is running".into(),
                ));
            }
            if !["build", "plan"].contains(&target.as_str()) {
                return Err(RequestError::Invalid(
                    "switch_agent supports build or plan".into(),
                ));
            }
            let current = state
                .get_or_create_session(session, request.session_id, None)
                .await
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            if !state.claim_session(&current.session_id).await {
                return Err(RequestError::Failed(
                    "session is busy on another connection".into(),
                ));
            }
            let replacement = registry::agent_profile(registry::ProfileInputs {
                model_catalog: Some(registry::ModelCatalog {
                    entries: Arc::new(state.bootstrap.models.clone()),
                    default: state.bootstrap.model_selector.clone(),
                }),
                name: &target,
                provider: current.provider.clone(),
                pool: state.bootstrap.pool.clone(),
                working_directory: state.bootstrap.working_directory.clone().into(),
                additional_tools: state.bootstrap.mcp_tools.clone(),
                skills: state.bootstrap.skills.clone(),
                web_fetch: state.bootstrap.web_fetch.clone(),
                enforcement: state.bootstrap.enforcement,
                context_limit: Some(current.context_limit),
                agents: state.bootstrap.agents.clone(),
                session_id: &current.session_id,
            });
            let result = match replacement {
                Ok((tools, prompt, _)) => match gewu_storage::set_session_agent(
                    &state.bootstrap.pool,
                    &current.session_id,
                    &target,
                )
                .await
                {
                    Ok(()) => {
                        let mut agent = current.agent.lock().await;
                        let reminder =
                            registry::profile_reminder(Some(&current.agent_name), &target);
                        agent.set_profile(prompt, tools, reminder);
                        if let Err(e) = gewu_storage::set_reminder(
                            &state.bootstrap.pool,
                            &current.session_id,
                            agent.reminder(),
                        )
                        .await
                        {
                            tracing::error!("failed to persist reminder state after switch: {e}");
                        }
                        let tools = agent.list_tools();
                        *session = Some(ConnectionSession {
                            agent: current.agent.clone(),
                            session_id: current.session_id.clone(),
                            agent_name: target.clone(),
                            model: current.model.clone(),
                            provider: current.provider.clone(),
                            context_limit: current.context_limit,
                            tools: tools.clone(),
                            steering: current.steering.clone(),
                        });
                        state.sessions.lock().await.insert(
                            current.session_id.clone(),
                            session.as_ref().unwrap().clone(),
                        );
                        Ok(Handled::Reply(ServerMessage::AgentSwitched {
                            id: request.id,
                            session_id: current.session_id.clone(),
                            agent: target,
                            tools,
                        }))
                    }
                    Err(e) => Err(RequestError::Failed(e.to_string())),
                },
                Err(e) => Err(RequestError::Failed(e.to_string())),
            };
            state.busy_sessions.lock().await.remove(&current.session_id);
            result
        }
        Op::SwitchModel { model: target } => {
            if running.is_some() {
                return Err(RequestError::Failed(
                    "cannot switch model while a turn is running".into(),
                ));
            }
            let current = state
                .get_or_create_session(session, request.session_id, None)
                .await
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            if !state.claim_session(&current.session_id).await {
                return Err(RequestError::Failed(
                    "session is busy on another connection".into(),
                ));
            }
            let result = match state.bootstrap.models.get(&target) {
                Some(entry) => match gewu_storage::set_session_model(
                    &state.bootstrap.pool,
                    &current.session_id,
                    &target,
                )
                .await
                {
                    Ok(()) => {
                        {
                            let mut agent = current.agent.lock().await;
                            agent.set_llm(entry.provider.clone());
                            agent.context_limit = entry.context_limit;
                        }
                        let replacement = ConnectionSession {
                            agent: current.agent.clone(),
                            session_id: current.session_id.clone(),
                            agent_name: current.agent_name.clone(),
                            model: target.clone(),
                            provider: entry.provider.clone(),
                            context_limit: entry.context_limit,
                            tools: current.tools.clone(),
                            steering: current.steering.clone(),
                        };
                        *session = Some(replacement.clone());
                        state
                            .sessions
                            .lock()
                            .await
                            .insert(current.session_id.clone(), replacement);
                        Ok(Handled::Reply(ServerMessage::ModelSwitched {
                            id: request.id,
                            session_id: current.session_id.clone(),
                            model: target,
                            label: entry.label.clone(),
                        }))
                    }
                    Err(e) => Err(RequestError::Failed(e.to_string())),
                },
                None => Err(RequestError::Invalid(format!("unknown model: {target}"))),
            };
            state.busy_sessions.lock().await.remove(&current.session_id);
            result
        }
        Op::ListModels => Ok(Handled::Reply(ServerMessage::Models {
            id: request.id,
            models: state
                .bootstrap
                .models
                .iter()
                .map(|(selector, entry)| protocol::ModelSummary {
                    selector: selector.clone(),
                    label: entry.label.clone(),
                })
                .collect(),
        })),
        Op::Ping => Ok(Handled::Reply(ServerMessage::Pong { id: request.id })),

        // Auth is normally consumed by the connection loop's auth gate; if a
        // client authenticates twice, accept it idempotently.
        Op::Auth { .. } => Ok(Handled::Reply(ServerMessage::Auth {
            id: request.id,
            ok: true,
        })),

        Op::Info => Ok(Handled::Reply(ServerMessage::Info {
            id: request.id,
            model: state.bootstrap.model.clone(),
            provider: state.bootstrap.provider_name.clone(),
            working_directory: state.bootstrap.working_directory.clone(),
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
                    busy: false,
                    provider_usage: None,
                }));
            }
            let conn = state
                .get_or_create_session(session, request.session_id, request.agent)
                .await
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            // Never block on a running ask (the agent lock is held for the
            // whole ask): report "unknown" as zeros and flag the reply busy
            // so clients can tell this apart from "no session yet".
            let Ok(agent) = conn.agent.try_lock() else {
                return Ok(Handled::Reply(ServerMessage::Usage {
                    id: request.id,
                    used_tokens: 0,
                    context_limit: 0,
                    busy: true,
                    provider_usage: None,
                }));
            };
            let used_tokens: usize =
                gewu_core::compaction::active_context(agent.memory.messages()).estimate_tokens();
            let provider_usage = agent.provider_usage();
            Ok(Handled::Reply(ServerMessage::Usage {
                id: request.id,
                used_tokens: used_tokens as u64,
                context_limit: agent.context_limit as u64,
                busy: false,
                provider_usage: (provider_usage.prompt_tokens() > 0
                    || provider_usage.output_tokens > 0)
                    .then_some(provider_usage),
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

        Op::ListFiles { path } => {
            let root = &state.bootstrap.working_directory;
            let dir = match state.bootstrap.enforcement {
                PathEnforcement::None => {
                    let dir = std::path::Path::new(root).join(&path);
                    if !dir.is_dir() {
                        return Err(RequestError::Invalid("path is not a directory".into()));
                    }
                    dir
                }
                _ => {
                    let dir = gewu_tools::fs_utils::resolve_safe_path(root, &path)
                        .map_err(|e| RequestError::Invalid(e.to_string()))?;
                    let canonical_root = std::fs::canonicalize(root).map_err(|e| {
                        RequestError::Failed(format!("cannot resolve working directory: {e}"))
                    })?;
                    let canonical_dir = std::fs::canonicalize(&dir)
                        .map_err(|e| RequestError::Failed(format!("cannot read directory: {e}")))?;
                    if !canonical_dir.starts_with(&canonical_root) {
                        return Err(RequestError::Invalid(
                            "directory is outside the working directory".into(),
                        ));
                    }
                    if !canonical_dir.is_dir() {
                        return Err(RequestError::Invalid("path is not a directory".into()));
                    }
                    canonical_dir
                }
            };
            let mut entries = Vec::new();
            for entry in ignore::WalkBuilder::new(&dir)
                .max_depth(Some(1))
                .hidden(true)
                .git_ignore(true)
                .require_git(false)
                .build()
                .skip(1)
            {
                let entry = entry.map_err(|e| RequestError::Failed(e.to_string()))?;
                let Some(name) = entry.file_name().to_str() else {
                    continue;
                };
                let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
                entries.push(FileEntry {
                    name: name.to_string(),
                    is_dir,
                });
            }
            entries.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            Ok(Handled::Reply(ServerMessage::Files {
                id: request.id,
                path,
                entries,
            }))
        }

        Op::ReadFile { path } => {
            const MAX_PREVIEW_BYTES: usize = 512 * 1024;
            let root = &state.bootstrap.working_directory;
            let file = match state.bootstrap.enforcement {
                PathEnforcement::None => std::path::Path::new(root).join(&path),
                _ => {
                    let file = gewu_tools::fs_utils::resolve_safe_path(root, &path)
                        .map_err(|e| RequestError::Invalid(e.to_string()))?;
                    let canonical_root = std::fs::canonicalize(root).map_err(|e| {
                        RequestError::Failed(format!("cannot resolve working directory: {e}"))
                    })?;
                    let canonical_file = std::fs::canonicalize(&file)
                        .map_err(|e| RequestError::Failed(format!("cannot read file: {e}")))?;
                    if !canonical_file.starts_with(&canonical_root) {
                        return Err(RequestError::Invalid(
                            "file is outside the working directory".into(),
                        ));
                    }
                    canonical_file
                }
            };
            let metadata = std::fs::metadata(&file)
                .map_err(|e| RequestError::Failed(format!("cannot read file: {e}")))?;
            if !metadata.is_file() {
                return Err(RequestError::Invalid("path is not a file".into()));
            }
            let file = std::fs::File::open(&file)
                .map_err(|e| RequestError::Failed(format!("cannot read file: {e}")))?;
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(
                &mut std::io::Read::take(file, (MAX_PREVIEW_BYTES + 1) as u64),
                &mut bytes,
            )
            .map_err(|e| RequestError::Failed(format!("cannot read file: {e}")))?;
            let truncated = bytes.len() > MAX_PREVIEW_BYTES;
            bytes.truncate(MAX_PREVIEW_BYTES);
            if bytes.iter().take(8192).any(|byte| *byte == 0) {
                return Err(RequestError::Failed(
                    "binary files cannot be previewed".into(),
                ));
            }
            if truncated {
                while let Err(error) = std::str::from_utf8(&bytes) {
                    if error.error_len().is_some() {
                        break;
                    }
                    bytes.truncate(error.valid_up_to());
                }
            }
            let content = String::from_utf8(bytes)
                .map_err(|_| RequestError::Failed("file is not valid UTF-8 text".into()))?;
            Ok(Handled::Reply(ServerMessage::FileContent {
                id: request.id,
                path,
                content,
                truncated,
            }))
        }

        Op::WriteFile {
            path,
            content_base64,
        } => {
            const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;
            // Only a bare file name is accepted: uploads land in the
            // working-directory root, and the browser-provided name is
            // untrusted. Directory components are stripped, so traversal
            // like `../evil` collapses to `evil`.
            let name = std::path::Path::new(&path)
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty() && *name != "." && *name != "..")
                .ok_or_else(|| RequestError::Invalid("path must be a file name".into()))?;
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(content_base64.as_bytes())
                .map_err(|e| {
                    RequestError::Invalid(format!("content_base64 is not valid base64: {e}"))
                })?;
            if bytes.len() > MAX_UPLOAD_BYTES {
                return Err(RequestError::Invalid(format!(
                    "file exceeds the {} MB upload limit",
                    MAX_UPLOAD_BYTES / (1024 * 1024)
                )));
            }
            let canonical_root = std::fs::canonicalize(&state.bootstrap.working_directory)
                .map_err(|e| {
                    RequestError::Failed(format!("cannot resolve working directory: {e}"))
                })?;
            let target = canonical_root.join(name);
            if target.exists() {
                // Follow symlinks so an existing name cannot redirect the
                // write outside the working directory.
                let canonical_target = std::fs::canonicalize(&target).map_err(|e| {
                    RequestError::Failed(format!("cannot resolve target file: {e}"))
                })?;
                if !canonical_target.starts_with(&canonical_root) {
                    return Err(RequestError::Invalid(
                        "file is outside the working directory".into(),
                    ));
                }
            }
            std::fs::write(&target, &bytes)
                .map_err(|e| RequestError::Failed(format!("cannot write file: {e}")))?;
            Ok(Handled::Reply(ServerMessage::FileWritten {
                id: request.id,
                path: name.to_string(),
                bytes: bytes.len() as u64,
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
            state.cancel_turn(&session_id).await;
            if !state.claim_session(&session_id).await {
                return Err(RequestError::Failed("session is busy".into()));
            }
            let deleted = gewu_storage::delete_session(&state.bootstrap.pool, &session_id)
                .await
                .map_err(|e| RequestError::Failed(e.to_string()));
            if deleted.is_ok() {
                state.sessions.lock().await.remove(&session_id);
                state.turns.lock().await.remove(&session_id);
            }
            state.release_session(&session_id).await;
            deleted?;
            info!("Deleted session {}", session_id);
            Ok(Handled::Reply(ServerMessage::Deleted {
                id: request.id,
                session_id,
            }))
        }

        Op::Interrupt => {
            let target = request
                .session_id
                .or_else(|| session.as_ref().map(|s| s.session_id.clone()));
            let request_id = match target {
                Some(id) => state.cancel_turn(&id).await,
                None => None,
            };
            *running = None;
            Ok(Handled::Reply(ServerMessage::Interrupted {
                id: Some(request.id),
                request_id,
            }))
        }

        Op::Steer { text } => {
            let target = request
                .session_id
                .or_else(|| session.as_ref().map(|s| s.session_id.clone()));
            let Some(target) = target else {
                return Err(RequestError::Failed(
                    "no session: send 'ask' before steering".to_string(),
                ));
            };
            let slot = state
                .sessions
                .lock()
                .await
                .get(&target)
                .map(|entry| entry.steering.clone());
            let Some(slot) = slot else {
                return Err(RequestError::Failed(format!("unknown session '{target}'")));
            };
            let cached = { slot.lock().await.clone() };
            let steering = if let Some(handle) = cached {
                Some(handle)
            } else {
                state.sessions.lock().await.get(&target).and_then(|entry| {
                    entry
                        .agent
                        .try_lock()
                        .ok()
                        .map(|agent| agent.steering().clone())
                })
            };
            match steering {
                Some(steering) => {
                    steering
                        .send(text.clone())
                        .map_err(|e| RequestError::Failed(format!("steering failed: {e}")))?;
                    Ok(Handled::Reply(ServerMessage::Steered {
                        id: request.id,
                        session_id: target,
                        text,
                    }))
                }
                None => Err(RequestError::Failed(
                    "no ask is in progress on this session".to_string(),
                )),
            }
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
            // Claim the session across connections so two connections cannot
            // interleave writes into one conversation.
            if !state.claim_session(&session.session_id).await {
                return Err(RequestError::Failed(format!(
                    "session '{}' is busy with an ask on another connection",
                    session.session_id
                )));
            }
            // Recheck under the claim: a switch on another connection could
            // have completed between session validation and claim acquisition.
            match gewu_storage::session_agent(&state.bootstrap.pool, &session.session_id).await {
                Ok(agent) if agent == session.agent_name => {}
                result => {
                    state.release_session(&session.session_id).await;
                    return Err(RequestError::Failed(match result {
                        Err(e) => e.to_string(),
                        _ => "session agent changed; reconnect before continuing".into(),
                    }));
                }
            }
            let request_id = crate::bootstrap::new_session_id();
            if let Err(e) =
                gewu_storage::commit_chat_revert(&state.bootstrap.pool, &session.session_id).await
            {
                state.release_session(&session.session_id).await;
                return Err(RequestError::Failed(e.to_string()));
            }
            Ok(Handled::Ask {
                ready: ServerMessage::Ready {
                    id: request.id,
                    request_id: request_id.clone(),
                    session_id: session.session_id.clone(),
                    agent: session.agent_name.clone(),
                },
                request_id,
                session_id: session.session_id.clone(),
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
    pool: &sqlx::SqlitePool,
    mut emit: E,
) where
    E: FnMut(ServerMessage) -> Fut,
    Fut: std::future::Future<Output = Result<(), anyhow::Error>>,
{
    let mut agent = session.agent.lock().await;
    // Repair any history left inconsistent by a previous interrupt.
    patch_dangling_tool_calls(&mut agent).await;

    // Publish the steering handle for `steer` requests while this turn
    // holds the agent lock; cleared again when the turn ends or aborts.
    *session.steering.lock().await = Some(agent.steering().clone());

    let mut client_gone = false;
    let mut terminal_sent = false;
    {
        let mut stream: std::pin::Pin<
            Box<dyn futures::Stream<Item = anyhow::Result<AgentEvent>> + Send + '_>,
        > = if prompt.is_empty() {
            Box::pin(agent.resume())
        } else {
            Box::pin(agent.ask(prompt))
        };
        // Whether a terminal frame (`result` or `error`) has been sent. Only
        // the `Finish` event and stream errors are terminal; an `AgentEvent::Error`
        // can be mid-stream (e.g. one failed tool among several).
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
                client_gone = true; // client went away; drop the stream and release the lock
                break;
            }
        }
    }

    // The reminder cadence advanced during this turn (counter, pending
    // consumption); make the new state durable before releasing the agent.
    if let Err(e) = gewu_storage::set_reminder(pool, &session.session_id, agent.reminder()).await {
        tracing::error!("failed to persist reminder state after turn: {e}");
    }

    drop(agent);
    if client_gone {
        return;
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
async fn start_session_turn(
    state: Arc<ServerState>,
    session: ConnectionSession,
    request_id: String,
    prompt: String,
    ready: ServerMessage,
    subscriber: Option<Subscriber>,
    handoff_ordinary: bool,
) {
    let mut messages = session.agent.lock().await.memory.messages().to_vec();
    if !prompt.is_empty() {
        messages.push(Message::User {
            content: vec![protocol::ContentPart::Text {
                text: prompt.clone(),
            }],
        });
    }
    let (cancel, mut cancelled) = tokio::sync::oneshot::channel();
    let live = Arc::new(Mutex::new(LiveTurn {
        request_id: request_id.clone(),
        messages,
        frames: vec![],
        bytes: 0,
        seq: 0,
        busy: true,
        model: session.model.clone(),
        done: Arc::new(tokio::sync::Notify::new()),
        subscribers: vec![],
        cancel: Some(cancel),
        terminal: None,
    }));
    // Register execution before acknowledging it: a failed socket write does
    // not strand a claimed session without its worker.
    let previous = state
        .turns
        .lock()
        .await
        .insert(session.session_id.clone(), live.clone());
    if let Some(previous) = previous {
        let mut previous = previous.lock().await;
        let snapshot = ServerMessage::SessionSnapshot {
            session_id: session.session_id.clone(),
            messages: live.lock().await.messages.clone(),
            frames: vec![],
            busy: true,
            seq: 0,
            model: Some(session.model.clone()),
        }
        .to_line();
        for observer in previous
            .subscribers
            .drain(..)
            .filter(|s| s.persistent || handoff_ordinary)
        {
            if subscriber
                .as_ref()
                .is_none_or(|s| !observer.out.same_channel(&s.out))
                && observer.send(&snapshot)
            {
                live.lock().await.subscribers.push(observer);
            }
        }
    }
    if let Some(subscriber) = subscriber
        && subscriber.send(&ready.to_line())
    {
        live.lock().await.subscribers.push(subscriber);
    }
    let worker_pool = state.bootstrap.pool.clone();
    tokio::spawn(async move {
        let worker_session = session.clone();
        let worker_live = live.clone();
        let mut worker = tokio::spawn(async move {
            execute_ask(
                &worker_session,
                &request_id,
                &prompt,
                &worker_pool,
                |message| {
                    let live = worker_live.clone();
                    async move {
                        let mut live = live.lock().await;
                        if message.is_ask_terminal().is_some() {
                            live.terminal = Some(message);
                        } else {
                            live.publish(message);
                        }
                        Ok(())
                    }
                },
            )
            .await;
        });
        let failure = tokio::select! {
            result = &mut worker => result.err().map(|e| format!("turn task failed: {e}")),
            _ = &mut cancelled => { worker.abort(); let _ = worker.await; Some("interrupted".into()) },
        };
        {
            let mut agent = session.agent.lock().await;
            patch_dangling_tool_calls(&mut agent).await;

            if failure.is_some() {
                drop_dangling_prompt(&mut agent).await;
                agent.steering().pause();
            }
            let mut live = live.lock().await;
            let terminal = failure
                .map(|error| ServerMessage::Error { id: None, error })
                .or_else(|| live.terminal.take());
            live.messages = agent.memory.messages().to_vec();
            live.frames.clear();
            live.bytes = 0;
            state.release_session(&session.session_id).await;
            live.busy = false;
            live.cancel = None;
            // Wake any interrupt awaiting finalization (see `cancel_turn`).
            live.done.notify_waiters();
            if let Some(terminal) = terminal {
                live.publish(terminal);
            }
            live.frames.clear();
            live.bytes = 0;
            let pending = agent.steering().should_resume();
            live.subscribers
                .retain(|s| (s.persistent || pending) && !s.out.is_closed());
            agent.steering().wake();
        }
    });
}

async fn watch_inbox(state: &Arc<ServerState>, session: &ConnectionSession) {
    let cached = { session.steering.lock().await.clone() };
    let inbox = match cached {
        Some(inbox) => inbox,
        None => session.agent.lock().await.steering().clone(),
    };
    *session.steering.lock().await = Some(inbox.clone());
    if !inbox.claim_watcher() {
        return;
    }
    let state = Arc::downgrade(state);
    let session_id = session.session_id.clone();
    tokio::spawn(async move {
        loop {
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(30), inbox.notified()).await;
            let Some(state) = state.upgrade() else { return };
            if !inbox.should_resume() {
                continue;
            }
            let _lifecycle = state.lifecycle.lock().await;
            let Some(session) = state.sessions.lock().await.get(&session_id).cloned() else {
                return;
            };
            if !state.claim_session(&session.session_id).await {
                continue;
            }
            if !inbox.should_resume() {
                state.release_session(&session.session_id).await;
                continue;
            }
            if gewu_storage::commit_chat_revert(&state.bootstrap.pool, &session_id)
                .await
                .is_err()
            {
                inbox.pause();
                state.release_session(&session_id).await;
                continue;
            }
            start_session_turn(
                state.clone(),
                session.clone(),
                crate::bootstrap::new_session_id(),
                String::new(),
                ServerMessage::Ready {
                    id: Value::Null,
                    request_id: String::new(),
                    session_id: session.session_id.clone(),
                    agent: session.agent_name.clone(),
                },
                None,
                true,
            )
            .await;
        }
    });
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
    // Token-protected servers require an `auth` request first.
    let mut authenticated = state.token.is_none();
    let (failed, mut failure) = tokio::sync::watch::channel(false);
    // Completion signals from ask tasks. The loop-side sender keeps this
    // channel open when no ask is running, so recv() simply stays pending.
    let (_done_tx, mut done_rx) = mpsc::unbounded_channel::<()>();

    let result = loop {
        tokio::select! {
            incoming = incoming.recv() => {
                let Some(raw) = incoming else { break Ok(()) };
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

                // Gate every operation behind authentication when the server
                // runs with a token.
                if !authenticated {
                    match request.op {
                        Op::Auth { token } => {
                            // Deliberately naive comparison: the threat model
                            // is casual remote access, not timing attacks.
                            let ok = state.token.as_deref() == Some(token.as_str());
                            out.send(
                                ServerMessage::Auth { id: request.id, ok }.to_line(),
                            )
                            .await?;
                            if ok {
                                info!("client authenticated");
                                authenticated = true;
                            } else {
                                warn!("client failed authentication; closing connection");
                                break Ok(());
                            }
                        }
                        _ => {
                            out.send(
                                ServerMessage::Error {
                                    id: Some(request_id_value),
                                    error: "authentication required: send {\"op\":\"auth\",\"token\":\"...\"} first"
                                        .to_string(),
                                }
                                .to_line(),
                            )
                            .await?;
                        }
                    }
                    continue;
                }

                // An interrupt request aborts + awaits the running ask inside
                // handle_request, so the agent lock is released before any
                // further request is handled.
                let _lifecycle = state.lifecycle.lock().await;
                // Refresh cached metadata after another observer switches profiles.
                if let Some(current) = session.as_mut() {
                    if let Some(latest) = state.sessions.lock().await.get(&current.session_id) {
                        *current = latest.clone();
                    } else {
                        session = None;
                    }
                }
                let handled = handle_request(&state, &mut session, &mut running, request).await;
                if let Some(session) = &session {
                    watch_inbox(&state, session).await;
                }
                match handled {
                    Ok(Handled::Reply(msg)) => {
                        out.send(msg.to_line()).await?;
                        if let ServerMessage::Steered { session_id, .. } = &msg {
                            let turn = state.turns.lock().await.get(session_id).cloned();
                            if let Some(turn) = turn {
                                let mut live = turn.lock().await;
                                if !live.busy && !live.subscribers.iter().any(|s| s.out.same_channel(&out)) {
                                    live.subscribers.push(Subscriber {
                                        out: out.clone(), failed: failed.clone(), persistent: false,
                                    });
                                }
                            }
                        }
                    }
                    Ok(Handled::Ask { ready, request_id, session_id, prompt }) => {
                        let session = session
                            .clone()
                            .expect("session created by handle_request");
                        start_session_turn(state.clone(), session, request_id, prompt, ready,
                            Some(Subscriber { out: out.clone(), failed: failed.clone(), persistent: false }), false).await;
                        running = Some(RunningAsk { session_id });
                    }
                    Ok(Handled::Subscribe { id, session: current }) => {
                        out.send(ServerMessage::Ready {
                            id, request_id: String::new(), session_id: current.session_id.clone(),
                            agent: current.agent_name,
                        }.to_line()).await?;
                        attach_subscriber(&state, &current.session_id, Subscriber {
                            out: out.clone(), failed: failed.clone(), persistent: true,
                        }).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
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
                // The running ask finished on its own; release its claim.
                if let Some(running) = running.take() {
                    state.release_session(&running.session_id).await;
                }
            }
            _ = failure.changed() => {
                break Err(anyhow::anyhow!("subscriber fell behind; reconnect to restore a snapshot"));
            }
        }
    };

    // A socket is only an observer. Session-owned workers survive detachment.
    let turns: Vec<_> = state.turns.lock().await.values().cloned().collect();
    for turn in turns {
        turn.lock()
            .await
            .subscribers
            .retain(|subscriber| !subscriber.out.same_channel(&out));
    }
    result
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
    // Cap inbound messages instead of relying on tungstenite's 64 MB default.
    let mut config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    config.max_message_size = Some(MAX_LINE_BYTES);
    config.max_frame_size = Some(MAX_LINE_BYTES);
    let ws = tokio_tungstenite::accept_async_with_config(stream, Some(config))
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

    reader.abort();
    let _ = reader.await;
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

    // Reader task: pushes validated lines into `in_tx`, answering protocol
    // violations (over-long lines, invalid UTF-8) with an error frame so
    // the client learns why its line was dropped and can keep talking.
    let err_tx = out_tx.clone();
    let reader = tokio::spawn(tcp_reader(reader_half, in_tx, err_tx));

    let result = drive_connection(state, in_rx, out_tx).await;

    reader.abort();
    let _ = writer.await;
    if let Err(e) = &result {
        warn!("TCP client {} error: {}", peer, e);
    } else {
        info!("TCP client disconnected: {}", peer);
    }
    result
}

/// Read newline-delimited lines from `stream`, enforcing [`MAX_LINE_BYTES`]
/// and UTF-8 validity.
///
/// Valid lines are forwarded on `in_tx`. Over-long lines and lines that are
/// not valid UTF-8 are answered with an error frame on `err_tx` and skipped;
/// the stream keeps reading, so a bad line no longer kills the connection
/// and no line can buffer without bound.
async fn tcp_reader<R: AsyncRead + Unpin>(
    mut stream: R,
    in_tx: mpsc::Sender<String>,
    err_tx: mpsc::Sender<String>,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    // Set while the bytes of an over-long line are being discarded up to
    // (and including) its terminating newline.
    let mut discarding = false;

    let line_too_long = || {
        ServerMessage::Error {
            id: None,
            error: format!(
                "line exceeds the maximum of {} bytes; skipped",
                MAX_LINE_BYTES
            ),
        }
        .to_line()
    };

    loop {
        // Deliver complete buffered lines.
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line = buf.drain(..=pos).collect::<Vec<u8>>();
            let line = &line[..line.len() - 1]; // strip '\n'
            let line = line.strip_suffix(b"\r").unwrap_or(line); // tolerate CRLF
            if discarding {
                // Reached the end of an over-long line: resume normal reads.
                discarding = false;
                continue;
            }
            if line.len() > MAX_LINE_BYTES {
                if err_tx.send(line_too_long()).await.is_err() {
                    return;
                }
                continue;
            }
            match String::from_utf8(line.to_vec()) {
                Ok(text) => {
                    if in_tx.send(text).await.is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let msg = ServerMessage::Error {
                        id: None,
                        error: "line is not valid UTF-8; skipped".to_string(),
                    }
                    .to_line();
                    if err_tx.send(msg).await.is_err() {
                        return;
                    }
                }
            }
        }
        if discarding {
            buf.clear();
        }

        match stream.read(&mut chunk).await {
            Ok(0) => return, // EOF
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // Enforce the cap between newlines. When the pending bytes
                // exceed the limit with no newline in sight, drop them and
                // keep discarding until the line ends (the error frame is
                // sent once, here). When a newline is already buffered,
                // leave the data alone: the delivery loop will reject the
                // over-long line and keep any following lines intact.
                if !discarding && buf.len() > MAX_LINE_BYTES && !buf.contains(&b'\n') {
                    discarding = true;
                    buf.clear();
                    if err_tx.send(line_too_long()).await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                warn!("TCP read error: {}", e);
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use gewu_core::types::{ContentPart, ToolContext, ToolResult};
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // -- Test doubles -------------------------------------------------------

    /// Pauses at a precise provider boundary without timing-based sleeps.
    struct CompactionLlm {
        block_call: Option<usize>,
        calls: AtomicUsize,
        blocked: tokio::sync::Notify,
        requests: std::sync::Mutex<Vec<Vec<Message>>>,
    }

    #[async_trait::async_trait]
    impl gewu_core::traits::LlmProvider for CompactionLlm {
        fn build(
            &self,
            messages: &[Message],
            _tools: &[Arc<dyn gewu_core::traits::Tool>],
        ) -> std::result::Result<Value, Box<dyn Error + Send + Sync>> {
            self.requests.lock().unwrap().push(messages.to_vec());
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
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(async_stream::try_stream! {
                yield gewu_core::types::ProviderEvent::ContentDelta { delta: "summary or answer".into() };
                if self.block_call == Some(call) {
                    self.blocked.notify_one();
                    std::future::pending::<()>().await;
                }
                yield gewu_core::types::ProviderEvent::Finish { reason: Some("stop".into()) };
            }))
        }

        async fn list_models(
            &self,
        ) -> std::result::Result<Vec<String>, Box<dyn Error + Send + Sync>> {
            Ok(vec![])
        }
    }

    struct TestLlm {
        behavior: LlmBehavior,
        /// Artificial delay before yielding events (makes asks observably
        /// long-running for interrupt tests).
        delay_ms: u64,
        /// Interior-mutable count of `send` calls (`send` takes `&self`).
        /// The agent prepends a system prompt to the message history, so
        /// `messages.len()` cannot identify the conversation turn; count
        /// provider calls instead.
        calls: AtomicUsize,
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
                calls: AtomicUsize::new(0),
            })
        }
        fn tool_call() -> Arc<Self> {
            Arc::new(Self {
                behavior: LlmBehavior::ToolCall,
                delay_ms: 0,
                calls: AtomicUsize::new(0),
            })
        }
        /// Tool-calling LLM that sleeps before responding: long enough for
        /// interrupt requests to reliably arrive mid-ask.
        fn slow_tool_call() -> Arc<Self> {
            Arc::new(Self {
                behavior: LlmBehavior::ToolCall,
                delay_ms: 500,
                calls: AtomicUsize::new(0),
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
            _request: Value,
        ) -> std::result::Result<
            futures::stream::BoxStream<
                'a,
                std::result::Result<gewu_core::types::ProviderEvent, Box<dyn Error + Send + Sync>>,
            >,
            Box<dyn Error + Send + Sync>,
        > {
            if self.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let events: Vec<gewu_core::types::ProviderEvent> = match self.behavior {
                LlmBehavior::Plain => vec![
                    gewu_core::types::ProviderEvent::ContentDelta {
                        delta: "Hello from test".to_string(),
                    },
                    gewu_core::types::ProviderEvent::Finish {
                        reason: Some("stop".to_string()),
                    },
                ],
                LlmBehavior::ToolCall if call == 1 => vec![
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

    /// LLM that records the user prompts it is asked with, and delays before
    /// responding (so asks are interruptible). Used to observe exactly what
    /// history reaches the model.
    struct RecordingLlm {
        delay_ms: u64,
        user_prompts: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingLlm {
        fn slow() -> Arc<Self> {
            Arc::new(Self {
                delay_ms: 500,
                user_prompts: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn prompts(&self) -> Vec<String> {
            self.user_prompts.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl gewu_core::traits::LlmProvider for RecordingLlm {
        fn build(
            &self,
            messages: &[Message],
            _tools: &[Arc<dyn gewu_core::traits::Tool>],
        ) -> std::result::Result<Value, Box<dyn Error + Send + Sync>> {
            for message in messages {
                if let Message::User { content } = message {
                    let text = content
                        .iter()
                        .filter_map(|part| match part {
                            ContentPart::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    if !text.is_empty() {
                        self.user_prompts.lock().unwrap().push(text);
                    }
                }
            }
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
            if self.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            let s = async_stream::try_stream! {
                yield gewu_core::types::ProviderEvent::ContentDelta {
                    delta: "ok".to_string(),
                };
                yield gewu_core::types::ProviderEvent::Finish {
                    reason: Some("stop".to_string()),
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
        let llm: Arc<dyn gewu_core::traits::LlmProvider> = TestLlm::plain();
        let agent = gewu_core::Agent::builder()
            .llm(llm.clone())
            .with_tools(vec![])
            .build()
            .unwrap();
        let tools = gewu_core::ToolRegistry::list_tools(&agent);
        ConnectionSession {
            agent: Arc::new(Mutex::new(agent)),
            session_id: "sess-test".to_string(),
            agent_name: "build".to_string(),
            model: "test-model".to_string(),
            provider: llm,
            context_limit: 256_000,
            tools,
            steering: Arc::new(Mutex::new(None)),
        }
    }

    fn tool_session() -> ConnectionSession {
        let tool: Arc<dyn gewu_core::traits::Tool> = Arc::new(EchoTool);
        let llm: Arc<dyn gewu_core::traits::LlmProvider> = TestLlm::tool_call();
        let agent = gewu_core::Agent::builder()
            .llm(llm.clone())
            .with_tools(vec![tool])
            .build()
            .unwrap();
        let tools = gewu_core::ToolRegistry::list_tools(&agent);
        ConnectionSession {
            agent: Arc::new(Mutex::new(agent)),
            session_id: "sess-test".to_string(),
            agent_name: "build".to_string(),
            model: "test-model".to_string(),
            provider: llm,
            context_limit: 256_000,
            tools,
            steering: Arc::new(Mutex::new(None)),
        }
    }

    async fn collect_ask(session: &ConnectionSession, prompt: &str) -> Vec<String> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel::<String>(256);
        let handle = tokio::spawn({
            let session = session.clone();
            let prompt = prompt.to_string();
            async move {
                execute_ask(&session, "req-test", &prompt, &pool, |msg| {
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

    /// Read frames until the ask's terminal `result`, skipping streamed
    /// events (the agent emits a `usage` snapshot at every turn boundary).
    async fn recv_result(rx: &mut mpsc::Receiver<String>) -> serde_json::Value {
        loop {
            let msg = recv_json(rx).await;
            match msg["type"].as_str() {
                Some("result") => return msg,
                Some("event") => {}
                other => panic!("unexpected frame while waiting for the result: {other:?} {msg}"),
            }
        }
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

    #[tokio::test]
    async fn ask_streams_usage_event_before_result() {
        let session = plain_session();
        let lines = collect_ask(&session, "hi").await;

        let is_usage = |line: &str| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v.get("event").cloned())
                .map(|e| e["type"] == "usage")
                .unwrap_or(false)
        };
        let usage_index = lines
            .iter()
            .position(|l| is_usage(l))
            .unwrap_or_else(|| panic!("expected a usage event, got {:?}", lines));
        assert!(
            lines[usage_index].contains("used_tokens")
                && lines[usage_index].contains("context_limit"),
            "usage event line: {}",
            lines[usage_index]
        );
        let result_index = lines
            .iter()
            .position(|l| l.contains(r#""type":"result""#))
            .expect("ask should end with a result");
        assert!(usage_index < result_index, "usage must precede the result");
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
                reasoning: None,
                replay: None,
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
                reasoning: None,
                replay: None,
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
        let line = tokio::time::timeout(std::time::Duration::from_secs(5), out_rx.recv())
            .await
            .expect("server message timed out")
            .expect("server message");
        serde_json::from_str(&line).expect("valid JSON line")
    }

    #[tokio::test]
    async fn compaction_lifecycle_persists_requests_and_survives_interrupts() {
        for block_call in [None, Some(0), Some(1)] {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                let provider = Arc::new(CompactionLlm {
                    block_call,
                    calls: AtomicUsize::new(0),
                    blocked: tokio::sync::Notify::new(),
                    requests: std::sync::Mutex::new(vec![]),
                });
                let state = server_state_with(provider.clone());
                let session = state
                    .get_or_create_session(&mut None, Some("compact".into()), None)
                    .await
                    .unwrap();
                let original = Message::User {
                    content: vec![ContentPart::Text {
                        text: "archived-original".repeat(3200),
                    }],
                };
                {
                    let mut agent = session.agent.lock().await;
                    agent.context_limit = 24_000;
                    agent.memory.add_message(original.clone()).await;
                }
                let (input, mut output, connection) = spawn_connection(state.clone()).await;
                input
                    .send(
                        ClientRequest::ask(1, "continue")
                            .with_session("compact")
                            .to_line(),
                    )
                    .await
                    .unwrap();
                assert_eq!(recv_json(&mut output).await["type"], "ready");
                if block_call.is_some() {
                    provider.blocked.notified().await;
                    input
                        .send(
                            ClientRequest::interrupt(2)
                                .with_session("compact")
                                .to_line(),
                        )
                        .await
                        .unwrap();
                    loop {
                        if recv_json(&mut output).await["type"] == "interrupted" {
                            break;
                        }
                    }
                } else {
                    loop {
                        if recv_json(&mut output).await["type"] == "result" {
                            break;
                        }
                    }
                }
                let loaded =
                    gewu_storage::SqliteMemory::new(state.bootstrap.pool.clone(), "compact".into())
                        .await
                        .unwrap();
                use gewu_core::traits::Memory;
                assert!(
                    loaded.messages().contains(&original),
                    "original must survive reload"
                );
                let checkpoints = loaded
                    .messages()
                    .iter()
                    .filter(|m| matches!(m, Message::Compaction { .. }))
                    .count();
                assert_eq!(checkpoints, usize::from(block_call != Some(0)));
                let requests = provider.requests.lock().unwrap().clone();
                if block_call != Some(0) {
                    assert_eq!(
                        requests.len(),
                        2,
                        "summary and post-compaction model request"
                    );
                    let request = serde_json::to_string(&requests[1]).unwrap();
                    assert!(request.contains("<conversation-checkpoint>"));
                    assert!(!request.contains("archived-original"));
                    let context = gewu_core::compaction::active_context(loaded.messages());
                    assert!(context.estimate_tokens() < 1000);
                }
                drop(input);
                connection.await.unwrap().unwrap();
            })
            .await
            .expect("compaction lifecycle must complete");
        }
    }

    #[tokio::test]
    async fn observer_receives_turns_started_after_idle_subscription() {
        let state = server_state_with(TestLlm::plain());
        let (observer, mut updates, observer_task) = spawn_connection(state.clone()).await;
        observer
            .send(ClientRequest::subscribe_session(1, "observed").to_line())
            .await
            .unwrap();
        assert_eq!(recv_json(&mut updates).await["type"], "ready");
        assert_eq!(recv_json(&mut updates).await["busy"], false);
        let (writer, mut replies, writer_task) = spawn_connection(state.clone()).await;
        for id in 2..4 {
            writer
                .send(
                    ClientRequest::ask(id, "hello")
                        .with_session("observed")
                        .to_line(),
                )
                .await
                .unwrap();
            assert_eq!(recv_json(&mut replies).await["type"], "ready");
            let snapshot = recv_json(&mut updates).await;
            assert_eq!(snapshot["type"], "session_snapshot");
            assert_eq!(snapshot["busy"], true);
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if recv_json(&mut updates).await["type"] == "result" {
                        break;
                    }
                }
                loop {
                    if recv_json(&mut replies).await["type"] == "result" {
                        break;
                    }
                }
            })
            .await
            .unwrap();
        }
        drop(observer);
        drop(writer);
        drop(updates);
        drop(replies);
        observer_task.await.unwrap().unwrap();
        writer_task.await.unwrap().unwrap();
        assert!(
            state.turns.lock().await["observed"]
                .lock()
                .await
                .subscribers
                .is_empty()
        );
    }

    #[tokio::test]
    async fn observer_survives_a_revert_and_sees_the_next_turn() {
        let state = server_state_with(TestLlm::plain());
        let (observer, mut updates, observer_task) = spawn_connection(state.clone()).await;
        observer
            .send(ClientRequest::subscribe_session(1, "observed").to_line())
            .await
            .unwrap();
        assert_eq!(recv_json(&mut updates).await["type"], "ready");
        assert_eq!(recv_json(&mut updates).await["busy"], false);
        let (writer, mut replies, writer_task) = spawn_connection(state.clone()).await;

        // One completed exchange gives the revert something to remove.
        writer
            .send(
                ClientRequest::ask(2, "hello")
                    .with_session("observed")
                    .to_line(),
            )
            .await
            .unwrap();
        assert_eq!(recv_json(&mut replies).await["type"], "ready");
        let snapshot = recv_json(&mut updates).await;
        assert_eq!(snapshot["busy"], true);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if recv_json(&mut updates).await["type"] == "result" {
                    break;
                }
            }
            loop {
                if recv_json(&mut replies).await["type"] == "result" {
                    break;
                }
            }
        })
        .await
        .unwrap();

        // Revert the exchange; the observer sees the truncated history
        // (only the system prompt remains).
        writer
            .send(
                ClientRequest::revert_chat(3, None, false)
                    .with_session("observed")
                    .to_line(),
            )
            .await
            .unwrap();
        let reverted = recv_json(&mut updates).await;
        assert_eq!(reverted["type"], "session_snapshot");
        assert_eq!(reverted["busy"], false);
        let history = reverted["messages"].as_array().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["role"], "system");
        assert_eq!(recv_json(&mut replies).await["type"], "session");

        // A turn started after the revert must reach the observer again —
        // its snapshot carries the new user prompt, which is what the GUI
        // renders as the message.
        writer
            .send(
                ClientRequest::ask(4, "again")
                    .with_session("observed")
                    .to_line(),
            )
            .await
            .unwrap();
        assert_eq!(recv_json(&mut replies).await["type"], "ready");
        let snapshot = recv_json(&mut updates).await;
        assert_eq!(snapshot["type"], "session_snapshot");
        assert_eq!(snapshot["busy"], true);
        let messages = snapshot["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2, "system prompt plus the new user prompt");
        assert_eq!(messages[1]["role"], "user");
        assert!(
            serde_json::to_value(&messages[1])
                .unwrap()
                .to_string()
                .contains("again")
        );

        // Keep both receivers alive until the second turn finishes. Closing
        // them after the initial snapshot races with the remaining events
        // and intentionally trips the subscriber-failure path.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if recv_json(&mut updates).await["type"] == "result" {
                    break;
                }
            }
            loop {
                if recv_json(&mut replies).await["type"] == "result" {
                    break;
                }
            }
        })
        .await
        .unwrap();

        drop(observer);
        drop(writer);
        drop(updates);
        drop(replies);
        observer_task.await.unwrap().unwrap();
        writer_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn slow_subscriber_signals_connection_failure() {
        let (out, _receiver) = mpsc::channel(1);
        let (failed, failure) = tokio::sync::watch::channel(false);
        let subscriber = Subscriber {
            out,
            failed,
            persistent: true,
        };
        assert!(subscriber.send("first"));
        assert!(!subscriber.send("second"));
        assert!(*failure.borrow());
    }

    #[tokio::test]
    async fn disconnected_turn_can_be_observed_and_cancelled_from_new_connection() {
        let state = server_state_with(TestLlm::slow_tool_call());
        let (input, mut output, connection) = spawn_connection(state.clone()).await;
        input
            .send(ClientRequest::ask(1, "work").to_line())
            .await
            .unwrap();
        let ready = recv_json(&mut output).await;
        let id = ready["session_id"].as_str().unwrap().to_owned();
        drop(input);
        drop(output);
        connection.await.unwrap().unwrap();
        assert!(state.busy_sessions.lock().await.contains(&id));

        let (input, mut output, connection) = spawn_connection(state.clone()).await;
        input
            .send(ClientRequest::subscribe_session(2, &id).to_line())
            .await
            .unwrap();
        assert_eq!(recv_json(&mut output).await["type"], "ready");
        let snapshot = recv_json(&mut output).await;
        assert_eq!(snapshot["type"], "session_snapshot");
        assert_eq!(snapshot["busy"], true);
        assert_eq!(snapshot["messages"][0]["role"], "user");
        input
            .send(ClientRequest::interrupt(3).with_session(&id).to_line())
            .await
            .unwrap();
        loop {
            let message = recv_json(&mut output).await;
            if message["type"] == "interrupted" {
                break;
            }
        }
        assert!(!state.busy_sessions.lock().await.contains(&id));
        drop(input);
        drop(output);
        connection.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn detached_turn_finishes_without_a_subscriber() {
        let state = server_state_with(TestLlm::plain());
        let (input, mut output, connection) = spawn_connection(state.clone()).await;
        input
            .send(ClientRequest::ask(1, "hello").to_line())
            .await
            .unwrap();
        let ready = recv_json(&mut output).await;
        let id = ready["session_id"].as_str().unwrap().to_owned();
        drop(input);
        drop(output);
        connection.await.unwrap().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !state.busy_sessions.lock().await.contains(&id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut current = None;
        let session = state
            .get_or_create_session(&mut current, Some(id), None)
            .await
            .unwrap();
        assert!(
            session
                .agent
                .lock()
                .await
                .memory
                .messages()
                .iter()
                .any(|m| matches!(m, Message::Assistant { .. }))
        );
    }

    fn server_state_with(provider: Arc<dyn gewu_core::traits::LlmProvider>) -> Arc<ServerState> {
        server_state_with_token(provider, None)
    }

    fn server_state_with_token(
        provider: Arc<dyn gewu_core::traits::LlmProvider>,
        token: Option<&str>,
    ) -> Arc<ServerState> {
        Arc::new(ServerState {
            bootstrap: Bootstrap {
                provider: provider.clone(),
                pool: sqlx::sqlite::SqlitePoolOptions::new()
                    .connect_lazy("sqlite::memory:")
                    .unwrap(),
                mcp_tools: Vec::new(),
                skills: Vec::new(),
                web_fetch: Arc::default(),
                log_writer: None,
                model: "test-model".to_string(),
                provider_name: "test".to_string(),
                working_directory: "/tmp/gewu-test".to_string(),
                enforcement: PathEnforcement::None,
                context_limit: None,
                models: BTreeMap::from([(
                    "test-model".to_string(),
                    crate::bootstrap::ModelEntry {
                        provider: provider.clone(),
                        provider_name: "test".to_string(),
                        label: "test-model".to_string(),
                        context_limit: 256_000,
                    },
                )]),
                model_selector: "test-model".to_string(),
                agents: Arc::new(BTreeMap::new()),
            },
            token: token.map(str::to_string),
            busy_sessions: Mutex::new(HashSet::new()),
            sessions: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            lifecycle: Mutex::new(()),
        })
    }

    #[tokio::test]
    async fn chat_revert_updates_live_history_and_undo_restores_it() {
        let state = server_state_with(TestLlm::plain());
        let mut session = None;
        let current = state
            .get_or_create_session(&mut session, Some("revert-test".into()), None)
            .await
            .unwrap();
        {
            let mut agent = current.agent.lock().await;
            for text in ["first", "second"] {
                agent
                    .memory
                    .add_message(Message::User {
                        content: vec![gewu_core::types::ContentPart::Text { text: text.into() }],
                    })
                    .await;
            }
        }
        let mut running = None;
        handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::revert_chat(1, None, false),
        )
        .await
        .unwrap();
        assert_eq!(current.agent.lock().await.memory.messages().len(), 1);
        handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::revert_chat(2, None, true),
        )
        .await
        .unwrap();
        assert_eq!(current.agent.lock().await.memory.messages().len(), 2);
    }

    /// Reverting past the message carrying the plan reminder must re-arm it:
    /// otherwise the profile stays plan (restricted tools) while the model
    /// loses every plan-mode signal — the original README-edit regression.
    #[tokio::test]
    async fn revert_past_the_switch_reminder_re_arms_it() {
        let state = server_state_with(TestLlm::plain());
        let mut session = None;
        let current = state
            .get_or_create_session(
                &mut session,
                Some("revert-plan".into()),
                Some("plan".into()),
            )
            .await
            .unwrap();
        {
            let mut agent = current.agent.lock().await;
            // Simulate the first plan turn: reminder consumed into history.
            let (full, sparse) = agent.reminder().texts();
            agent.set_reminder(gewu_core::agent::Reminder::continue_at(full, sparse, 1));
            agent
                .memory
                .add_message(Message::User {
                    content: vec![
                        gewu_core::types::ContentPart::Text {
                            text: "Update README.md".into(),
                        },
                        gewu_core::types::ContentPart::Reminder {
                            text: "Plan mode is active".into(),
                        },
                    ],
                })
                .await;
        }
        let mut running = None;
        handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::revert_chat(0, None, false),
        )
        .await
        .unwrap();
        let agent = current.agent.lock().await;
        assert!(
            agent
                .memory
                .messages()
                .iter()
                .all(|m| !matches!(m, Message::User { .. })),
            "revert to turn 0 truncates every user message"
        );
        let reminder = agent.reminder();
        assert!(
            reminder.pending().is_some(),
            "a fresh reminder must be pending after the revert"
        );
        assert_eq!(reminder.user_turns(), 0);
    }

    /// When the retained history still carries the reminder, reverting must
    /// not re-arm it — the next ask would append a duplicate.
    #[tokio::test]
    async fn revert_with_retained_reminder_does_not_duplicate_it() {
        let state = server_state_with(TestLlm::plain());
        let mut session = None;
        let current = state
            .get_or_create_session(
                &mut session,
                Some("revert-keep".into()),
                Some("plan".into()),
            )
            .await
            .unwrap();
        {
            let mut agent = current.agent.lock().await;
            let (full, sparse) = agent.reminder().texts();
            agent.set_reminder(gewu_core::agent::Reminder::continue_at(full, sparse, 2));
            agent
                .memory
                .add_message(Message::User {
                    content: vec![
                        gewu_core::types::ContentPart::Text {
                            text: "first".into(),
                        },
                        gewu_core::types::ContentPart::Reminder {
                            text: "Plan mode is active".into(),
                        },
                    ],
                })
                .await;
            agent
                .memory
                .add_message(Message::User {
                    content: vec![gewu_core::types::ContentPart::Text {
                        text: "second".into(),
                    }],
                })
                .await;
        }
        let mut running = None;
        // Revert to turn 1: the first user message (with its reminder) stays.
        handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::revert_chat(1, None, false),
        )
        .await
        .unwrap();
        let agent = current.agent.lock().await;
        assert_eq!(agent.memory.messages().len(), 1);
        let reminder = agent.reminder();
        assert!(
            reminder.pending().is_none(),
            "retained reminder must not be duplicated"
        );
        assert_eq!(
            reminder.user_turns(),
            1,
            "cadence realigns with retained history"
        );
    }

    #[tokio::test]
    async fn file_tree_ops_follow_enforcement_setting() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.txt");
        std::fs::write(&outside_file, "outside").unwrap();
        let outside_path = outside_file.to_string_lossy().to_string();
        let mut session = None;
        let mut running = None;

        // Default (None): paths outside the working root are served.
        let mut state = server_state_with(TestLlm::plain());
        {
            let bootstrap = &mut Arc::get_mut(&mut state).unwrap().bootstrap;
            bootstrap.working_directory = root.path().to_string_lossy().to_string();
            bootstrap.enforcement = PathEnforcement::None;
        }
        let handled = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::read_file(1, &outside_path),
        )
        .await
        .unwrap();
        match handled {
            Handled::Reply(ServerMessage::FileContent { content, .. }) => {
                assert_eq!(content, "outside")
            }
            _ => panic!("unexpected reply"),
        }

        // Contain: the same request is rejected as outside the root.
        Arc::get_mut(&mut state).unwrap().bootstrap.enforcement = PathEnforcement::Contain;
        let result = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::read_file(2, &outside_path),
        )
        .await;
        assert!(result.is_err(), "expected containment rejection");
    }

    #[tokio::test]
    async fn write_file_writes_within_working_root() {
        let root = tempfile::tempdir().unwrap();
        let mut session = None;
        let mut running = None;
        let mut state = server_state_with(TestLlm::plain());
        {
            let bootstrap = &mut Arc::get_mut(&mut state).unwrap().bootstrap;
            bootstrap.working_directory = root.path().to_string_lossy().to_string();
        }

        // Binary (non-UTF-8) payloads round-trip through base64.
        let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&payload);
        let handled = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::write_file(1, "data.bin", &encoded),
        )
        .await
        .unwrap();
        match handled {
            Handled::Reply(ServerMessage::FileWritten { path, bytes, .. }) => {
                assert_eq!(path, "data.bin");
                assert_eq!(bytes, payload.len() as u64);
            }
            _ => panic!("unexpected reply"),
        }
        assert_eq!(
            std::fs::read(root.path().join("data.bin")).unwrap(),
            payload
        );

        // Overwriting an existing file is allowed.
        let replaced = b"replacement";
        let encoded = base64::engine::general_purpose::STANDARD.encode(replaced);
        handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::write_file(2, "data.bin", &encoded),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("data.bin")).unwrap(),
            replaced
        );
    }

    #[tokio::test]
    async fn write_file_collapses_traversal_to_a_root_name() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("victim.txt"), "victim").unwrap();
        let mut session = None;
        let mut running = None;
        let mut state = server_state_with(TestLlm::plain());
        {
            let bootstrap = &mut Arc::get_mut(&mut state).unwrap().bootstrap;
            bootstrap.working_directory = root.path().to_string_lossy().to_string();
        }

        let encoded = base64::engine::general_purpose::STANDARD.encode(b"payload");
        handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::write_file(1, "../victim.txt", &encoded),
        )
        .await
        .unwrap();
        // The `..` component is stripped, so the write lands in the root and
        // the file outside it is untouched.
        assert_eq!(
            std::fs::read(root.path().join("victim.txt")).unwrap(),
            b"payload"
        );
        assert_eq!(
            std::fs::read(outside.path().join("victim.txt")).unwrap(),
            b"victim"
        );
    }

    #[tokio::test]
    async fn write_file_rejects_bad_names_and_payloads() {
        let root = tempfile::tempdir().unwrap();
        let mut session = None;
        let mut running = None;
        let mut state = server_state_with(TestLlm::plain());
        {
            let bootstrap = &mut Arc::get_mut(&mut state).unwrap().bootstrap;
            bootstrap.working_directory = root.path().to_string_lossy().to_string();
        }

        // `..` and empty names carry no file component to write.
        for name in ["..", ".", ""] {
            let result = handle_request(
                &state,
                &mut session,
                &mut running,
                ClientRequest::write_file(1, name, "aGk="),
            )
            .await;
            assert!(result.is_err(), "expected '{name}' to be rejected");
        }
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);

        // Non-base64 payloads are rejected before touching the disk.
        let result = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::write_file(2, "data.bin", "!!!not base64!!!"),
        )
        .await;
        assert!(result.is_err(), "expected invalid base64 to be rejected");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn write_file_rejects_oversize_payloads() {
        let root = tempfile::tempdir().unwrap();
        let mut session = None;
        let mut running = None;
        let mut state = server_state_with(TestLlm::plain());
        {
            let bootstrap = &mut Arc::get_mut(&mut state).unwrap().bootstrap;
            bootstrap.working_directory = root.path().to_string_lossy().to_string();
        }

        let big = vec![0u8; 50 * 1024 * 1024 + 1];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&big);
        let result = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::write_file(1, "big.bin", &encoded),
        )
        .await;
        let err = match result {
            Err(err) => err,
            Ok(_) => panic!("expected the upload cap to reject the payload"),
        };
        assert!(err.to_string().contains("upload limit"), "{err}");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn switching_profiles_keeps_live_memory() {
        let state = server_state_with(TestLlm::plain());
        let original = plain_session();
        original
            .agent
            .lock()
            .await
            .memory
            .add_message(Message::User {
                content: vec![gewu_core::types::ContentPart::Text {
                    text: "live memory not stored in SQLite".into(),
                }],
            })
            .await;
        let before = original.agent.lock().await.memory.messages().to_vec();
        let mut session = Some(original.clone());
        let mut running = None;
        for mode in ["plan", "build"] {
            handle_request(
                &state,
                &mut session,
                &mut running,
                ClientRequest::switch_agent(1, mode),
            )
            .await
            .unwrap();
            let current = session.as_ref().unwrap();
            assert!(Arc::ptr_eq(&original.agent, &current.agent));
            assert_eq!(
                current.agent.lock().await.memory.messages(),
                before.as_slice()
            );
            assert_eq!(current.agent_name, mode);
        }
    }

    #[tokio::test]
    async fn switching_profiles_preserves_session_and_persists_mode() {
        let state = server_state_with(TestLlm::plain());
        let (tx, mut rx, handle) = spawn_connection(state.clone()).await;
        tx.send(ClientRequest::switch_agent(1, "plan").to_line())
            .await
            .unwrap();
        let plan = recv_json(&mut rx).await;
        assert_eq!(plan["type"], "agent_switched");
        assert_eq!(plan["agent"], "plan");
        let id = plan["session_id"].as_str().unwrap().to_string();
        assert!(
            plan["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "bash")
        );
        assert!(
            plan["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "task")
        );
        assert!(
            !plan["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "write_file")
        );
        assert_eq!(
            gewu_storage::session_agent(&state.bootstrap.pool, &id)
                .await
                .unwrap(),
            "plan"
        );
        tx.send(ClientRequest::switch_agent(2, "build").to_line())
            .await
            .unwrap();
        let build = recv_json(&mut rx).await;
        assert_eq!(build["session_id"], id);
        assert_eq!(build["agent"], "build");
        assert!(
            build["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "bash")
        );
        tx.send(ClientRequest::switch_agent(3, "unknown").to_line())
            .await
            .unwrap();
        assert_eq!(recv_json(&mut rx).await["type"], "error");
        drop(tx);
        handle.await.unwrap().unwrap();
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
        let _result = recv_result(&mut out_rx).await;

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
        assert_eq!(usage["context_limit"], 256_000);

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
                web_fetch: Arc::default(),
                log_writer: None,
                model: "test-model".to_string(),
                provider_name: "test".to_string(),
                working_directory: "/tmp/gewu-test".to_string(),
                enforcement: PathEnforcement::None,
                context_limit: None,
                models: BTreeMap::from([(
                    "test-model".to_string(),
                    crate::bootstrap::ModelEntry {
                        provider: TestLlm::plain(),
                        provider_name: "test".to_string(),
                        label: "test-model".to_string(),
                        context_limit: 256_000,
                    },
                )]),
                model_selector: "test-model".to_string(),
                agents: Arc::new(BTreeMap::new()),
            },
            token: None,
            busy_sessions: Mutex::new(HashSet::new()),
            sessions: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            lifecycle: Mutex::new(()),
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
        let result = recv_result(&mut out_rx).await;
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
        let result = recv_result(&mut out_rx).await;
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
    async fn list_sessions_hides_subagents_and_delete_cascades() {
        let state = server_state_with(TestLlm::plain());
        let (in_tx, mut out_rx, handle) = spawn_connection(state.clone()).await;

        // Produce a stored session.
        in_tx
            .send(r#"{"id": 1, "op": "ask", "prompt": "hello"}"#.to_string())
            .await
            .unwrap();
        let _ready = recv_json(&mut out_rx).await;
        let _event = recv_json(&mut out_rx).await;
        let result = recv_result(&mut out_rx).await;
        let session_id = result["session_id"].as_str().unwrap().to_string();

        // Simulate the task tool spawning two subagent sessions.
        let user = gewu_protocol::Message::User {
            content: vec![gewu_protocol::ContentPart::Text {
                text: "subagent work".to_string(),
            }],
        };
        for name in ["sub-a", "sub-b"] {
            gewu_storage::record_subagent_session(
                &state.bootstrap.pool,
                name,
                &session_id,
                "explore",
            )
            .await
            .unwrap();
            sqlx::query("INSERT INTO memory_messages (session_id, content) VALUES (?, ?)")
                .bind(name)
                .bind(serde_json::to_string(&user).unwrap())
                .execute(&state.bootstrap.pool)
                .await
                .unwrap();
        }

        in_tx
            .send(r#"{"id": 2, "op": "list_sessions"}"#.to_string())
            .await
            .unwrap();
        let sessions = recv_json(&mut out_rx).await;
        let listed: Vec<&str> = sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session_id"].as_str().unwrap())
            .collect();
        assert_eq!(
            listed,
            vec![session_id.as_str()],
            "subagent sessions must not be listed"
        );

        // Deleting the conversation removes its subagent transcripts too.
        in_tx
            .send(format!(
                r#"{{"id": 3, "op": "delete_session", "session_id": "{session_id}"}}"#
            ))
            .await
            .unwrap();
        assert_eq!(recv_json(&mut out_rx).await["type"], "deleted");
        in_tx
            .send(r#"{"id": 4, "op": "get_session", "session_id": "sub-a"}"#.to_string())
            .await
            .unwrap();
        let empty = recv_json(&mut out_rx).await;
        assert_eq!(empty["messages"].as_array().unwrap().len(), 0);

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn connection_rejects_second_ask_while_running() {
        // A slow tool-calling agent keeps the ask busy across two LLM turns,
        // long enough for the second ask to arrive while the first is still
        // running (a zero-delay ask can finish before it is even read).
        let state = server_state_with(TestLlm::slow_tool_call());
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
        // The rejection error is emitted by the connection loop while the
        // running ask streams events into the same channel from another
        // task, so drain messages until the error shows up. A `result`
        // first would mean the second ask was accepted, i.e. not rejected.
        loop {
            let msg = recv_json(&mut out_rx).await;
            match msg["type"].as_str() {
                Some("error") => {
                    assert!(
                        msg["error"]
                            .as_str()
                            .unwrap()
                            .contains("already in progress"),
                        "unexpected error: {}",
                        msg["error"]
                    );
                    break;
                }
                Some("result") => {
                    panic!("second ask should be rejected while the first is running")
                }
                _ => continue,
            }
        }

        // Let the first ask finish.
        loop {
            if recv_json(&mut out_rx).await["type"] == "result" {
                break;
            }
        }

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn connection_steer_injects_message_into_running_ask() {
        // The slow LLM delays before its first response, so the steer lands
        // while the turn is mid-flight. RecordingLlm records the user
        // prompts it is asked with, proving the steered text reached the
        // model as a user message on the follow-up request.
        let llm = RecordingLlm::slow();
        let state = server_state_with(llm.clone());
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        in_tx
            .send(r#"{"id": 1, "op": "ask", "prompt": "go"}"#.to_string())
            .await
            .unwrap();
        let ready = recv_json(&mut out_rx).await;
        assert_eq!(ready["type"], "ready");
        let session_id = ready["session_id"].as_str().unwrap().to_string();

        in_tx
            .send(r#"{"id": 2, "op": "steer", "text": "focus on tests"}"#.to_string())
            .await
            .unwrap();

        // The steer ack may interleave with streamed events; wait for it.
        loop {
            let msg = recv_json(&mut out_rx).await;
            match msg["type"].as_str() {
                Some("steered") => {
                    assert_eq!(msg["id"], 2);
                    assert_eq!(msg["text"], "focus on tests");
                    assert_eq!(msg["session_id"], session_id.as_str());
                    break;
                }
                Some("result") => panic!("steer ack must arrive before the ask completes"),
                _ => continue,
            }
        }

        // The steered message extends the run; wait for the final result.
        loop {
            if recv_json(&mut out_rx).await["type"] == "result" {
                break;
            }
        }

        // The agent streamed the steered message as an event...
        let prompts = llm.prompts();
        assert!(
            prompts.iter().any(|p| p.contains("focus on tests")),
            "steered text never reached the model: {prompts:?}"
        );
        assert_eq!(prompts.len(), 2, "steer must trigger a follow-up request");

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn idle_steer_resumes_for_the_last_request_client() {
        let state = server_state_with(TestLlm::plain());
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        // No session exists yet.
        in_tx
            .send(r#"{"id": 1, "op": "steer", "text": "hello"}"#.to_string())
            .await
            .unwrap();
        let msg = recv_json(&mut out_rx).await;
        assert_eq!(msg["type"], "error");
        assert!(msg["error"].as_str().unwrap().contains("no session"));

        // Run one ask to idle, then steer again: the session resumes automatically.
        in_tx
            .send(r#"{"id": 2, "op": "ask", "prompt": "go"}"#.to_string())
            .await
            .unwrap();
        loop {
            if recv_json(&mut out_rx).await["type"] == "result" {
                break;
            }
        }
        in_tx
            .send(r#"{"id": 3, "op": "steer", "text": "late"}"#.to_string())
            .await
            .unwrap();
        let ack = tokio::time::timeout(std::time::Duration::from_secs(5), recv_json(&mut out_rx))
            .await
            .expect("steer acknowledgement timed out");
        assert_eq!(ack["type"], "steered");
        let mut resumed_terminal = false;
        for _ in 0..16 {
            let msg =
                tokio::time::timeout(std::time::Duration::from_secs(5), recv_json(&mut out_rx))
                    .await
                    .expect("idle resumed turn timed out");
            if msg["type"] == "result" || msg["type"] == "error" {
                resumed_terminal = true;
                break;
            }
        }
        assert!(resumed_terminal);

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn idle_background_completion_reaches_persistent_subscriber() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let state = server_state_with(TestLlm::plain());
            let (in_tx, mut out_rx, handle) = spawn_connection(state.clone()).await;
            in_tx
                .send(r#"{"id":1,"op":"subscribe_session"}"#.into())
                .await
                .unwrap();
            let ready = recv_json(&mut out_rx).await;
            let id = ready["session_id"].as_str().unwrap();
            assert_eq!(recv_json(&mut out_rx).await["type"], "session_snapshot");
            let session = state.sessions.lock().await.get(id).unwrap().clone();
            let inbox = session.steering.lock().await.clone().unwrap();
            inbox.notify(gewu_core::agent::BackgroundNotification {
                source: "workflow".into(),
                id: "completed-run".into(),
                status: "completed".into(),
                output: "untrusted output".into(),
            });
            loop {
                let message = recv_json(&mut out_rx).await;
                if message["type"] == "result" {
                    assert_eq!(message["response"], "Hello from test");
                    break;
                }
            }
            assert!(!inbox.is_pending());
            drop(in_tx);
            handle.await.unwrap().unwrap();
        })
        .await
        .expect("idle background notification timed out");
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
        let mut interrupted = recv_json(&mut out_rx).await;
        if interrupted["type"] == "error" {
            interrupted = recv_json(&mut out_rx).await;
        }
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
            model: "test-model".to_string(),
            provider: Arc::new(ToolCallLoopLlm),
            context_limit: 256_000,
            tools: vec!["echo".to_string()],
            steering: Arc::new(Mutex::new(None)),
        };

        let (tx, mut rx) = mpsc::channel::<String>(256);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let handle = tokio::spawn({
            let session = session.clone();
            async move {
                execute_ask(&session, "req-test", "go", &pool, |msg| {
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

    // -- usage busy flag (Bug 2) ---------------------------------------------

    #[tokio::test]
    async fn usage_reports_busy_during_running_ask() {
        let state = server_state_with(TestLlm::slow_tool_call());
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        in_tx
            .send(r#"{"id": 1, "op": "ask", "prompt": "go"}"#.to_string())
            .await
            .unwrap();
        let ready = recv_json(&mut out_rx).await;
        assert_eq!(ready["type"], "ready");

        // While the ask holds the agent lock, usage is 0/0 *and* busy.
        in_tx
            .send(r#"{"id": 2, "op": "usage"}"#.to_string())
            .await
            .unwrap();
        let mut usage = None;
        for _ in 0..10 {
            let msg = recv_json(&mut out_rx).await;
            if msg["type"] == "usage" {
                usage = Some(msg);
                break;
            }
        }
        let usage = usage.expect("usage reply while ask running");
        assert_eq!(usage["used_tokens"], 0);
        assert_eq!(usage["context_limit"], 0);
        assert_eq!(usage["busy"], true, "busy ask must be flagged: {usage}");

        // No session at all stays 0/0 with busy: false.
        let state2 = server_state_with(TestLlm::plain());
        let (in_tx2, mut out_rx2, handle2) = spawn_connection(state2).await;
        in_tx2
            .send(r#"{"id": 1, "op": "usage"}"#.to_string())
            .await
            .unwrap();
        let usage = recv_json(&mut out_rx2).await;
        assert_eq!(usage["used_tokens"], 0);
        assert_eq!(usage["context_limit"], 0);
        assert_eq!(usage["busy"], false);

        drop(in_tx);
        let _ = handle.await.unwrap();
        drop(in_tx2);
        let _ = handle2.await.unwrap();
    }

    // -- TCP reader hardening (Bugs 3 and 6) ---------------------------------

    #[tokio::test]
    async fn tcp_reader_skips_invalid_utf8_and_keeps_reading() {
        let (mut client, server) = tokio::io::duplex(65_536);
        let (in_tx, mut in_rx) = mpsc::channel::<String>(16);
        let (err_tx, mut err_rx) = mpsc::channel::<String>(16);
        tokio::spawn(tcp_reader(server, in_tx, err_tx));

        use tokio::io::AsyncWriteExt;
        client
            .write_all(b"{\"id\":1,\"op\":\"ping\"}\n")
            .await
            .unwrap();
        client.write_all(b"\xff\xfe bad\n").await.unwrap();
        client
            .write_all(b"{\"id\":2,\"op\":\"ping\"}\n")
            .await
            .unwrap();

        let good = in_rx.recv().await.unwrap();
        assert!(good.contains("\"id\":1"), "{good}");
        let err = err_rx.recv().await.unwrap();
        assert!(err.contains("UTF-8"), "{err}");
        // The connection is still usable after the bad line.
        let good2 = in_rx.recv().await.unwrap();
        assert!(good2.contains("\"id\":2"), "{good2}");
    }

    #[tokio::test]
    async fn tcp_reader_rejects_overlong_lines_and_keeps_reading() {
        let (mut client, server) = tokio::io::duplex(65_536);
        let (in_tx, mut in_rx) = mpsc::channel::<String>(16);
        let (err_tx, mut err_rx) = mpsc::channel::<String>(16);
        tokio::spawn(tcp_reader(server, in_tx, err_tx));

        use tokio::io::AsyncWriteExt;
        // A line far over the cap, written in pieces, then a normal line.
        let pad = vec![b'x'; MAX_LINE_BYTES + 4096];
        client.write_all(&pad).await.unwrap();
        client.write_all(b"\n").await.unwrap();
        client
            .write_all(b"{\"id\":2,\"op\":\"ping\"}\n")
            .await
            .unwrap();

        let err = err_rx.recv().await.unwrap();
        assert!(err.contains("maximum"), "{err}");
        let good = in_rx.recv().await.unwrap();
        assert!(good.contains("\"id\":2"), "{good}");
        // Exactly one error frame for the one bad line.
        assert!(err_rx.try_recv().is_err());
    }

    // -- auth (Bug 5) ---------------------------------------------------------

    #[tokio::test]
    async fn token_protected_server_requires_auth_first() {
        let state = server_state_with_token(TestLlm::plain(), Some("secret"));
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        // Any other op is rejected until auth succeeds.
        in_tx
            .send(r#"{"id": 1, "op": "ping"}"#.to_string())
            .await
            .unwrap();
        let err = recv_json(&mut out_rx).await;
        assert_eq!(err["type"], "error");
        assert!(err["error"].as_str().unwrap().contains("authentication"));

        // Correct token unlocks the connection.
        in_tx
            .send(r#"{"id": 2, "op": "auth", "token": "secret"}"#.to_string())
            .await
            .unwrap();
        let auth = recv_json(&mut out_rx).await;
        assert_eq!(auth["type"], "auth");
        assert_eq!(auth["ok"], true);
        assert_eq!(auth["id"], 2);

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
    async fn token_protected_server_rejects_wrong_token_and_closes() {
        let state = server_state_with_token(TestLlm::plain(), Some("secret"));
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        in_tx
            .send(r#"{"id": 1, "op": "auth", "token": "wrong"}"#.to_string())
            .await
            .unwrap();
        let auth = recv_json(&mut out_rx).await;
        assert_eq!(auth["type"], "auth");
        assert_eq!(auth["ok"], false);

        // The connection is closed after a failed auth attempt.
        let closed = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
        assert!(closed.is_ok(), "connection should close after bad auth");
        assert!(closed.unwrap().unwrap().is_ok());
    }

    // -- session binding (Bug 9) ----------------------------------------------

    #[tokio::test]
    async fn session_mismatch_is_rejected() {
        let state = server_state_with(TestLlm::plain());
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        // Bind the connection to session s1.
        in_tx
            .send(r#"{"id": 1, "op": "ask", "prompt": "hi", "session_id": "s1"}"#.to_string())
            .await
            .unwrap();
        let ready = recv_json(&mut out_rx).await;
        assert_eq!(ready["session_id"], "s1");
        loop {
            if recv_json(&mut out_rx).await["type"] == "result" {
                break;
            }
        }

        // A different session_id is now an error, not a silent ignore.
        in_tx
            .send(r#"{"id": 2, "op": "list_tools", "session_id": "other"}"#.to_string())
            .await
            .unwrap();
        let err = recv_json(&mut out_rx).await;
        assert_eq!(err["type"], "error");
        assert!(
            err["error"].as_str().unwrap().contains("session mismatch"),
            "{err}"
        );

        // So is a different agent.
        in_tx
            .send(r#"{"id": 3, "op": "list_tools", "agent": "explore"}"#.to_string())
            .await
            .unwrap();
        let err = recv_json(&mut out_rx).await;
        assert_eq!(err["type"], "error");
        assert!(
            err["error"].as_str().unwrap().contains("agent mismatch"),
            "{err}"
        );

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    // -- cross-connection session locking (Bug 4) ------------------------------

    #[tokio::test]
    async fn second_connection_cannot_ask_a_busy_session() {
        let state = server_state_with(TestLlm::slow_tool_call());
        let (in_a, mut out_a, handle_a) = spawn_connection(state.clone()).await;
        let (in_b, mut out_b, handle_b) = spawn_connection(state.clone()).await;

        // Connection A starts an ask, creating session s1.
        in_a.send(r#"{"id": 1, "op": "ask", "prompt": "go", "session_id": "s1"}"#.to_string())
            .await
            .unwrap();
        let ready = recv_json(&mut out_a).await;
        assert_eq!(ready["type"], "ready");

        // Connection B targets the same session and is rejected.
        in_b.send(r#"{"id": 1, "op": "ask", "prompt": "me too", "session_id": "s1"}"#.to_string())
            .await
            .unwrap();
        let mut rejected = false;
        for _ in 0..10 {
            let msg = recv_json(&mut out_b).await;
            if msg["type"] == "error" {
                assert!(msg["error"].as_str().unwrap().contains("busy"), "{msg}");
                rejected = true;
                break;
            }
        }
        assert!(rejected, "second connection should be rejected");

        // A finishes; the claim is released.
        loop {
            if recv_json(&mut out_a).await["type"] == "result" {
                break;
            }
        }

        // B can now run an ask on the same session.
        in_b.send(r#"{"id": 2, "op": "ask", "prompt": "me too", "session_id": "s1"}"#.to_string())
            .await
            .unwrap();
        let ready_b = recv_json(&mut out_b).await;
        assert_eq!(
            ready_b["type"], "ready",
            "claim should be released: {ready_b}"
        );
        loop {
            if recv_json(&mut out_b).await["type"] == "result" {
                break;
            }
        }

        drop(in_a);
        drop(in_b);
        handle_a.await.unwrap().unwrap();
        handle_b.await.unwrap().unwrap();
    }

    // -- interrupted prompt handling (Bug 11) ---------------------------------

    #[tokio::test]
    async fn interrupt_drops_prompt_without_output() {
        let mut agent = gewu_core::Agent::builder()
            .llm(TestLlm::plain())
            .with_tools(vec![])
            .build()
            .unwrap();

        // Simulate an ask that was aborted before the model responded: the
        // prompt is the last message in the history.
        agent
            .memory
            .add_message(Message::User {
                content: vec![ContentPart::Text {
                    text: "write an essay".to_string(),
                }],
            })
            .await;

        drop_dangling_prompt(&mut agent).await;

        assert!(
            agent
                .memory
                .messages()
                .iter()
                .all(|m| !matches!(m, Message::User { .. })),
            "the unanswered prompt should be dropped: {:?}",
            agent.memory.messages()
        );
    }

    #[tokio::test]
    async fn drop_prompt_keeps_exchanges_that_produced_output() {
        let mut agent = gewu_core::Agent::builder()
            .llm(TestLlm::plain())
            .with_tools(vec![])
            .build()
            .unwrap();

        // An aborted exchange that already ran a tool: user, assistant tool
        // call, and (after the interrupt patch) the synthetic tool result.
        agent
            .memory
            .add_message(Message::User {
                content: vec![ContentPart::Text {
                    text: "do the thing".to_string(),
                }],
            })
            .await;
        agent
            .memory
            .add_message(Message::Assistant {
                content: None,
                reasoning: None,
                replay: None,
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
        drop_dangling_prompt(&mut agent).await;

        let messages = agent.memory.messages();
        assert_eq!(messages.len(), 3, "history with output is kept");
        assert!(matches!(messages[0], Message::User { .. }));
        assert!(matches!(messages.last().unwrap(), Message::Tool { .. }));
    }

    #[tokio::test]
    async fn interrupt_drops_prompt_across_full_connection() {
        // The next ask must not be steered by an interrupted prompt that
        // never produced output (Bug 11).
        let llm = RecordingLlm::slow();
        let state = server_state_with(llm.clone() as Arc<dyn gewu_core::traits::LlmProvider>);
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        in_tx
            .send(r#"{"id": 1, "op": "ask", "prompt": "write an essay"}"#.to_string())
            .await
            .unwrap();
        let ready = recv_json(&mut out_rx).await;
        assert_eq!(ready["type"], "ready");

        // Interrupt before the (delayed) LLM produced anything.
        in_tx
            .send(r#"{"id": 2, "op": "interrupt"}"#.to_string())
            .await
            .unwrap();
        let mut interrupted = recv_json(&mut out_rx).await;
        if interrupted["type"] == "error" {
            interrupted = recv_json(&mut out_rx).await;
        }
        assert_eq!(interrupted["type"], "interrupted");

        // The next ask's LLM request must not contain the aborted prompt.
        in_tx
            .send(r#"{"id": 3, "op": "ask", "prompt": "Say ok"}"#.to_string())
            .await
            .unwrap();
        loop {
            if recv_json(&mut out_rx).await["type"] == "result" {
                break;
            }
        }

        let prompts = llm.prompts();
        assert!(
            !prompts.iter().any(|p| p.contains("essay")),
            "aborted prompt leaked into the next LLM request: {prompts:?}"
        );
        assert!(
            prompts.iter().any(|p| p.contains("Say ok")),
            "the new prompt should reach the LLM: {prompts:?}"
        );

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }
}
