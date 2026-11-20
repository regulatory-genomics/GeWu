//! Server mode for gewu-server.
//!
//! Lets other programs connect to a running GeWu agent, send prompts, and
//! receive streamed agent events. Two transports carry the agent protocol,
//! and a third serves downloads:
//!
//! - **WebSocket** (port 8420): text frames, one JSON message per frame.
//! - **TCP** (port 8421): newline-delimited JSON, one message per line.
//! - **HTTP** (port 8422): streaming file and folder-ZIP downloads.
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
use gewu_protocol::{
    DirectoryEntry, FileEntry, WorkflowNodeState, WorkflowRunState, WorkspaceSummary,
};
use registry::create_agent;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};
use tokio::task::{AbortHandle, JoinSet};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{info, warn};

use crate::bootstrap::Bootstrap;
use crate::registry;
pub use protocol::{ClientRequest, Op, RequestError, ServerMessage};

mod download;

/// Default bind address for all listeners.
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// Default port for the WebSocket listener.
pub const DEFAULT_WS_PORT: u16 = 8420;
/// Default port for the TCP listener.
pub const DEFAULT_TCP_PORT: u16 = 8421;
/// Default port for the HTTP file-download listener.
pub const DEFAULT_HTTP_PORT: u16 = 8422;

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
    /// Broadcast channel of live workflow-run progress. Every workflow
    /// manager the process builds publishes into it; the forwarder below
    /// routes each run's updates to its owning session's subscribers.
    workflow_progress: tokio::sync::broadcast::Sender<gewu_tools::workflow::RunProgress>,
    /// Latest live state of each session's workflow runs, replayed to
    /// (re)connecting subscribers.
    workflows: Mutex<HashMap<String, BTreeMap<String, WorkflowRunState>>>,
    /// Pending `question` tool requests. Questions are live-only: entries
    /// disappear when answered, dismissed, or when the asking tool is
    /// dropped (interrupt).
    questions: Arc<gewu_core::QuestionBroker>,
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
    /// The explicit reasoning effort the turn runs on, echoed in snapshots.
    reasoning_effort: Option<String>,
    /// Notified (waiters) when `busy` flips back to false at turn
    /// finalization, so interrupts await completion without spinning.
    done: Arc<tokio::sync::Notify>,
    subscribers: Vec<Subscriber>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    interrupted: bool,
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
            reasoning_effort: live.reasoning_effort.clone(),
        };
        subscriber
            .out
            .send(snapshot.to_line())
            .await
            .map_err(|_| RequestError::Failed("subscriber disconnected".into()))?;
        // Replay the latest workflow state so a reconnecting UI can draw
        // live graphs again. Sent under the turn lock, so no live publish
        // can slip between the snapshot and its workflow state.
        let runs: Vec<WorkflowRunState> = state
            .workflows
            .lock()
            .await
            .get(session_id)
            .map(|runs| runs.values().cloned().collect())
            .unwrap_or_default();
        for run in runs {
            subscriber
                .out
                .send(
                    ServerMessage::WorkflowProgress {
                        session_id: session_id.to_owned(),
                        run,
                    }
                    .to_line(),
                )
                .await
                .map_err(|_| RequestError::Failed("subscriber disconnected".into()))?;
        }
        live.subscribers
            .retain(|s| !s.out.same_channel(&subscriber.out));
        live.subscribers.push(subscriber);
        return Ok(());
    }
    let Some(session) = state.sessions.lock().await.get(session_id).cloned() else {
        return Err(RequestError::Failed("unknown session".into()));
    };
    let messages = session.agent.lock().await.memory.messages().to_vec();
    let live = Arc::new(Mutex::new(LiveTurn {
        request_id: String::new(),
        messages,
        frames: vec![],
        bytes: 0,
        seq: 0,
        busy: false,
        model: session.model.clone(),
        reasoning_effort: session.reasoning_effort.clone(),
        done: Arc::new(tokio::sync::Notify::new()),
        subscribers: Vec::new(),
        cancel: None,
        interrupted: false,
        terminal: None,
    }));
    // Hold the fresh turn's lock across the snapshot/replay/registration
    // below, so a publisher that finds the turn cannot emit a frame ahead
    // of its snapshot.
    let mut live_guard = live.lock().await;
    state
        .turns
        .lock()
        .await
        .insert(session_id.to_owned(), live.clone());
    subscriber
        .out
        .send(
            ServerMessage::SessionSnapshot {
                session_id: session_id.to_owned(),
                messages: live_guard.messages.clone(),
                frames: vec![],
                busy: false,
                seq: 0,
                model: Some(session.model.clone()),
                reasoning_effort: session.reasoning_effort.clone(),
            }
            .to_line(),
        )
        .await
        .map_err(|_| RequestError::Failed("subscriber disconnected".into()))?;
    let runs: Vec<WorkflowRunState> = state
        .workflows
        .lock()
        .await
        .get(session_id)
        .map(|runs| runs.values().cloned().collect())
        .unwrap_or_default();
    for run in runs {
        subscriber
            .out
            .send(
                ServerMessage::WorkflowProgress {
                    session_id: session_id.to_owned(),
                    run,
                }
                .to_line(),
            )
            .await
            .map_err(|_| RequestError::Failed("subscriber disconnected".into()))?;
    }
    live_guard.subscribers.push(subscriber);
    Ok(())
}

/// Workflow runs kept per session for replay to reconnecting subscribers.
const MAX_TRACKED_WORKFLOWS: usize = 16;

fn count_u32(value: usize) -> u32 {
    value.min(u32::MAX as usize) as u32
}

/// Converts a manager progress event into its wire form.
fn workflow_run_state(progress: &gewu_tools::workflow::RunProgress) -> WorkflowRunState {
    WorkflowRunState {
        run_id: progress.run_id.clone(),
        name: progress.name.clone(),
        status: progress.status.label().to_string(),
        current_node: progress.current_node.clone(),
        tasks_done: count_u32(progress.tasks_done),
        tasks_failed: count_u32(progress.tasks_failed),
        tasks_attempted: count_u32(progress.tasks_attempted),
        nodes: progress
            .nodes
            .iter()
            .map(|node| WorkflowNodeState {
                id: node.id.clone(),
                status: node.status.clone(),
                done: count_u32(node.done),
                failed: count_u32(node.failed.len()),
            })
            .collect(),
        error: progress.error.clone(),
    }
}

/// Forwards workflow progress to the owning session's subscribers and keeps
/// the latest state for replay on reconnect. Recent finished runs are
/// retained (bounded per session) so a reload can still draw their graphs.
async fn forward_workflow_progress(
    state: Arc<ServerState>,
    mut progress: tokio::sync::broadcast::Receiver<gewu_tools::workflow::RunProgress>,
) {
    loop {
        let event = match progress.recv().await {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };
        let session_id = event.owner_session.clone();
        let run = workflow_run_state(&event);
        {
            let mut workflows = state.workflows.lock().await;
            let runs = workflows.entry(session_id.clone()).or_default();
            runs.insert(run.run_id.clone(), run.clone());
            if runs.len() > MAX_TRACKED_WORKFLOWS {
                let victim = runs
                    .iter()
                    .find(|(_, run)| run.status != "running")
                    .map(|(id, _)| id.clone())
                    .or_else(|| runs.keys().next().cloned());
                if let Some(id) = victim {
                    runs.remove(&id);
                }
            }
        }
        let turn = state.turns.lock().await.get(&session_id).cloned();
        if let Some(turn) = turn {
            turn.lock()
                .await
                .publish(ServerMessage::WorkflowProgress { session_id, run });
        }
    }
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
    /// The session's explicit reasoning effort; `None` uses the model's
    /// configured default, `provider-default` the provider's own default.
    reasoning_effort: Option<String>,
    /// Resolved context window (tokens) of the session's model.
    context_limit: usize,
    /// Cached tool names, taken from the agent at creation. The tool set is
    /// fixed for the life of the session, so `list_tools` can be answered
    /// without locking the agent (which would block while an ask runs).
    tools: Vec<String>,
    inbox: Arc<SessionInbox>,
    /// The session's selected filesystem root.
    workspace: WorkspaceSummary,
}

#[derive(Default)]
struct SessionInbox {
    steering: gewu_core::Steering,
    watcher: OnceLock<AbortHandle>,
}

impl SessionInbox {
    fn stop(&self) {
        if let Some(watcher) = self.watcher.get() {
            watcher.abort();
        }
    }
}

impl Drop for SessionInbox {
    fn drop(&mut self) {
        self.stop();
    }
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
    http_port: u16,
    token: Option<String>,
) -> Result<()> {
    let ws_addr: SocketAddr = format!("{}:{}", host, ws_port)
        .parse()
        .context("Invalid WebSocket bind address")?;
    let tcp_addr: SocketAddr = format!("{}:{}", host, tcp_port)
        .parse()
        .context("Invalid TCP bind address")?;
    let http_addr: SocketAddr = format!("{}:{}", host, http_port)
        .parse()
        .context("Invalid HTTP bind address")?;

    let ws_listener = TcpListener::bind(ws_addr)
        .await
        .with_context(|| format!("Failed to bind WebSocket listener on {}", ws_addr))?;
    let tcp_listener = TcpListener::bind(tcp_addr)
        .await
        .with_context(|| format!("Failed to bind TCP listener on {}", tcp_addr))?;
    let http_listener = TcpListener::bind(http_addr)
        .await
        .with_context(|| format!("Failed to bind HTTP listener on {}", http_addr))?;

    let (workflow_progress, workflow_rx) = tokio::sync::broadcast::channel(256);
    let state = Arc::new(ServerState {
        bootstrap,
        token,
        busy_sessions: Mutex::new(HashSet::new()),
        sessions: Mutex::new(HashMap::new()),
        turns: Mutex::new(HashMap::new()),
        workflow_progress,
        workflows: Mutex::new(HashMap::new()),
        questions: Arc::new(gewu_core::QuestionBroker::new()),
        lifecycle: Mutex::new(()),
    });

    info!("gewu server listening:");
    info!("  WebSocket: ws://{}", ws_addr);
    info!("  TCP (newline-delimited JSON): {}", tcp_addr);
    info!("  HTTP downloads: http://{}", http_addr);
    if state.token.is_some() {
        info!("authentication required (clients send the 'auth' op first)");
    }

    // Per-connection handlers are spawned into a shared tracking set so
    // shutdown can abort and drain them together with the listeners.
    let mut connections: JoinSet<()> = JoinSet::new();
    let (conn_tx, mut conn_rx) = mpsc::unbounded_channel();

    let mut tasks = JoinSet::new();
    tasks.spawn(forward_workflow_progress(state.clone(), workflow_rx));
    tasks.spawn(accept_loop(
        ws_listener,
        Transport::WebSocket,
        conn_tx.clone(),
    ));
    tasks.spawn(accept_loop(tcp_listener, Transport::Tcp, conn_tx.clone()));
    let http_router = download::router(state.clone()).merge(state.bootstrap.plugins.data_router());
    tasks.spawn(async move {
        if let Err(error) = axum::serve(http_listener, http_router).await {
            warn!("HTTP listener failed: {error}");
        }
    });
    drop(conn_tx); // accept loops hold the remaining senders

    // Run until Ctrl+C (or until both accept loops exit).
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        tokio::select! {
            result = &mut ctrl_c => {
                result.context("Failed to listen for Ctrl+C")?;
                info!("Received Ctrl+C, shutting down");
                break;
            }
            connection = conn_rx.recv() => {
                match connection {
                    Some((stream, peer, transport)) => {
                        let state = state.clone();
                        connections.spawn(async move {
                            let result = match transport {
                                Transport::WebSocket => handle_ws_connection(stream, peer, state).await,
                                Transport::Tcp => handle_tcp_connection(stream, peer, state).await,
                            };
                            if let Err(e) = result {
                                warn!("Connection {} error: {}", peer, e);
                            }
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
    state.bootstrap.plugins.close_all().await?;
    Ok(())
}

/// The transport a connection came in on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    WebSocket,
    Tcp,
}

/// Accept connections on one listener forever, forwarding each stream to the tracker.
async fn accept_loop(
    listener: TcpListener,
    transport: Transport,
    conn_tx: mpsc::UnboundedSender<(TcpStream, SocketAddr, Transport)>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                if conn_tx.send((stream, peer, transport)).is_err() {
                    break;
                }
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
    fn workspace_summaries(&self) -> Vec<WorkspaceSummary> {
        self.bootstrap
            .workspace_roots
            .iter()
            .map(|(id, path)| WorkspaceSummary {
                id: id.clone(),
                path: path.to_string_lossy().to_string(),
            })
            .collect()
    }

    fn workspace(&self, id: &str) -> Result<WorkspaceSummary> {
        if let Some(path) = self.bootstrap.workspace_roots.get(id) {
            return Ok(WorkspaceSummary {
                id: id.to_string(),
                path: path.to_string_lossy().to_string(),
            });
        }
        let Some(path) = id.strip_prefix("runtime:") else {
            anyhow::bail!("unknown workspace '{id}'")
        };
        let path = std::fs::canonicalize(path)
            .with_context(|| format!("cannot resolve runtime workspace '{path}'"))?;
        anyhow::ensure!(
            path.is_dir(),
            "workspace is not a directory: {}",
            path.display()
        );
        Ok(WorkspaceSummary {
            id: id.to_string(),
            path: path.to_string_lossy().to_string(),
        })
    }

    fn workspace_for_path(&self, path: impl AsRef<std::path::Path>) -> Result<WorkspaceSummary> {
        let path = std::fs::canonicalize(path.as_ref())
            .with_context(|| format!("cannot resolve directory: {}", path.as_ref().display()))?;
        anyhow::ensure!(path.is_dir(), "path is not a directory: {}", path.display());
        if let Some((id, _)) = self
            .bootstrap
            .workspace_roots
            .iter()
            .find(|(_, root)| root.as_path() == path.as_path())
        {
            return self.workspace(id);
        }
        Ok(WorkspaceSummary {
            id: format!("runtime:{}", path.display()),
            path: path.to_string_lossy().to_string(),
        })
    }

    fn browse_workspace(
        &self,
        requested: &str,
    ) -> Result<(String, Option<String>, Vec<DirectoryEntry>)> {
        let cwd = self
            .bootstrap
            .workspace_roots
            .get("cwd")
            .expect("cwd workspace is always configured");
        let requested_path = if requested.trim().is_empty() {
            cwd.clone()
        } else {
            let path = std::path::PathBuf::from(requested);
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        };
        let directory = std::fs::canonicalize(&requested_path)
            .with_context(|| format!("cannot resolve directory: {}", requested_path.display()))?;
        anyhow::ensure!(
            directory.is_dir(),
            "path is not a directory: {}",
            directory.display()
        );
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("cannot list directory: {}", directory.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.starts_with('.') || !path.is_dir() {
                continue;
            }
            entries.push(DirectoryEntry {
                name,
                path: path.to_string_lossy().to_string(),
            });
        }
        entries.sort_by_key(|a| a.name.to_lowercase());
        let parent = directory
            .parent()
            .map(|path| path.to_string_lossy().to_string());
        Ok((directory.to_string_lossy().to_string(), parent, entries))
    }

    /// Get or create the per-connection session.
    ///
    /// `session_id` and `agent_name` only apply when the session is first
    /// created; subsequent calls on the same connection reuse it. A later
    /// request naming a *different* `session_id` or `agent` is rejected so
    /// mismatches surface as errors instead of being silently ignored.
    async fn get_or_create_session(
        self: &Arc<Self>,
        current: &mut Option<ConnectionSession>,
        session_id: Option<String>,
        agent_name: Option<String>,
    ) -> Result<ConnectionSession> {
        self.get_or_create_session_with_workspace(current, session_id, agent_name, None)
            .await
    }

    /// Get or create a session, optionally selecting its workspace at creation
    /// time. The workspace hint is ignored once the session already exists.
    async fn get_or_create_session_with_workspace(
        self: &Arc<Self>,
        current: &mut Option<ConnectionSession>,
        session_id: Option<String>,
        agent_name: Option<String>,
        requested_workspace: Option<String>,
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

        let stored_id = gewu_storage::session_workspace(&self.bootstrap.pool, &session_id).await?;
        let use_requested_workspace = requested_workspace.is_some() && stored_id.is_none();
        let workspace_id = if use_requested_workspace {
            requested_workspace.expect("checked above")
        } else {
            stored_id.clone().unwrap_or_else(|| "cwd".into())
        };
        let workspace = if use_requested_workspace {
            self.workspace(&workspace_id)?
        } else {
            self.workspace(&workspace_id).unwrap_or_else(|error| {
                warn!(
                    "session {} has invalid workspace '{}': {}; using cwd",
                    session_id, workspace_id, error
                );
                self.workspace("cwd")
                    .expect("cwd workspace is always configured")
            })
        };
        let workspace_path = std::path::PathBuf::from(&workspace.path);

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
        let stored_effort =
            gewu_storage::session_reasoning_effort(&self.bootstrap.pool, &session_id).await?;
        let reasoning_effort = stored_effort.and_then(|effort| {
            if entry.supports_effort(&effort) {
                Some(effort)
            } else {
                warn!(
                    "session {} stored unsupported reasoning effort '{}'; using the model default",
                    session_id, effort
                );
                None
            }
        });
        let provider = entry
            .provider_for(reasoning_effort.as_deref())
            .ok_or_else(|| {
                anyhow::anyhow!("reasoning effort is unavailable for model '{model}'")
            })?;

        let (agent, _registry) = create_agent(
            registry::ProfileInputs {
                model_catalog: Some(registry::ModelCatalog {
                    entries: Arc::new(self.bootstrap.models.clone()),
                    default: self.bootstrap.model_selector.clone(),
                }),
                name: &agent_name,
                provider: provider.clone(),
                pool: self.bootstrap.pool.clone(),
                working_directory: workspace_path,
                additional_tools: self.bootstrap.mcp_tools.clone(),
                skills: self.bootstrap.skills.clone(),
                enforcement: self.bootstrap.enforcement,
                image_limits: self.bootstrap.image_limits,
                context_limit: Some(entry.context_limit),
                agents: self.bootstrap.agents.clone(),
                workflow_progress: Some(self.workflow_progress.clone()),
                question_broker: Some(self.questions.clone()),
                session_id: &session_id,
            },
            self.bootstrap.log_writer.clone(),
        )
        .await?;
        info!(
            "Created session {} (agent: {}, model: {}, effort: {})",
            session_id,
            agent_name,
            model,
            reasoning_effort.as_deref().unwrap_or("default")
        );

        let agent = Arc::new(Mutex::new(agent));
        {
            let mut agent = agent.lock().await;
            patch_dangling_tool_calls(&mut agent).await;
        }
        if use_requested_workspace {
            gewu_storage::set_session_workspace(&self.bootstrap.pool, &session_id, &workspace_id)
                .await?;
        }
        gewu_storage::set_session_agent(&self.bootstrap.pool, &session_id, &agent_name).await?;
        // Snapshot the tool list once; it never changes for this session.
        let tools = agent.lock().await.list_tools();

        let entry = ConnectionSession {
            agent,
            session_id,
            agent_name,
            model,
            provider,
            reasoning_effort,
            context_limit: entry.context_limit,
            tools,
            inbox: Arc::new(SessionInbox::default()),
            workspace,
        };
        *current = Some(entry.clone());
        sessions.insert(entry.session_id.clone(), entry.clone());
        entry.inbox.watcher.get_or_init(|| {
            tokio::spawn(watch_inbox(
                Arc::downgrade(self),
                entry.session_id.clone(),
                Arc::downgrade(&entry.inbox),
                entry.inbox.steering.clone(),
            ))
            .abort_handle()
        });
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

    /// Release a session claim and recheck any steering queued while busy.
    async fn release_session(&self, session_id: &str) {
        let sessions = self.sessions.lock().await;
        if self.busy_sessions.lock().await.remove(session_id)
            && let Some(session) = sessions.get(session_id)
        {
            session.inbox.steering.wake();
        }
    }

    async fn cancel_turn(&self, session_id: &str) -> Option<String> {
        let inbox = self
            .sessions
            .lock()
            .await
            .get(session_id)
            .map(|session| session.inbox.steering.clone());
        let turn = self.turns.lock().await.get(session_id).cloned();
        let Some(turn) = turn else {
            if let Some(inbox) = inbox {
                inbox.pause();
            }
            return None;
        };
        let request_id = {
            let mut turn = turn.lock().await;
            if let Some(inbox) = inbox {
                inbox.pause();
                turn.interrupted = true;
            }
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
        let _lifecycle = self.lifecycle.lock().await;
        for session in self.sessions.lock().await.values() {
            session.inbox.stop();
        }
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
        input: AskInput,
    },
    Subscribe {
        id: Value,
        session: ConnectionSession,
    },
}

/// One ask's text and its resolved attachments, threaded from the request
/// handler through the turn runner into the agent.
struct AskInput {
    prompt: String,
    media: Vec<protocol::ContentPart>,
}

/// Inline media cap per ask attachment (decoded bytes) — the same bound
/// `read_file` applies to media it extracts for the model.
const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

/// Whether an attachment's MIME type can be sent to the model as media.
fn attachment_mime_supported(mime_type: &str) -> bool {
    matches!(
        mime_type,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "application/pdf"
    )
}

/// Build the model-facing content part for an inline attachment.
fn attachment_part(mime_type: &str, bytes: Vec<u8>) -> protocol::ContentPart {
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    if mime_type == "application/pdf" {
        protocol::ContentPart::File {
            data,
            mime_type: mime_type.to_string(),
        }
    } else {
        protocol::ContentPart::Image {
            data,
            mime_type: mime_type.to_string(),
        }
    }
}

/// Resolve an ask's attachments into model-facing content parts.
///
/// Inline `data` attachments decode and pass through (size- and MIME-checked).
/// `path` attachments resolve inside the workspace root: supported media
/// within the size cap are inlined, everything else becomes a text reference
/// the agent can follow with `read_file`.
fn resolve_ask_attachments(
    root: &str,
    enforcement: PathEnforcement,
    store_dir: Option<&std::path::Path>,
    attachments: &[gewu_protocol::AskAttachment],
) -> Result<Vec<protocol::ContentPart>, RequestError> {
    let mut parts = Vec::new();
    for attachment in attachments {
        if let Some(data) = &attachment.data {
            use base64::Engine as _;
            if !attachment_mime_supported(&attachment.mime_type) {
                return Err(RequestError::Invalid(format!(
                    "attachment '{}' has unsupported MIME type '{}'",
                    attachment.name, attachment.mime_type
                )));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data.as_bytes())
                .map_err(|e| {
                    RequestError::Invalid(format!(
                        "attachment '{}' is not valid base64: {e}",
                        attachment.name
                    ))
                })?;
            if bytes.len() > MAX_ATTACHMENT_BYTES {
                return Err(RequestError::Invalid(format!(
                    "attachment '{}' exceeds the {} MB inline limit; upload it to the workspace and reference it by path instead",
                    attachment.name,
                    MAX_ATTACHMENT_BYTES / (1024 * 1024)
                )));
            }
            parts.push(attachment_part(&attachment.mime_type, bytes));
        } else if let Some(path) = &attachment.path {
            // Store paths win: an attached file uploaded through
            // `upload_attachment` shadows a same-named workspace file.
            let file = if let Some(dir) = store_dir {
                let candidate = dir.join(path);
                if candidate.is_file() {
                    candidate
                } else {
                    resolve_workspace_path(root, path, enforcement)?
                }
            } else {
                resolve_workspace_path(root, path, enforcement)?
            };
            let metadata = std::fs::metadata(&file)
                .map_err(|e| RequestError::Failed(format!("cannot read attachment: {e}")))?;
            if !metadata.is_file() {
                return Err(RequestError::Invalid(format!(
                    "attachment path '{path}' is not a file"
                )));
            }
            if attachment_mime_supported(&attachment.mime_type)
                && metadata.len() <= MAX_ATTACHMENT_BYTES as u64
            {
                let bytes = std::fs::read(&file)
                    .map_err(|e| RequestError::Failed(format!("cannot read attachment: {e}")))?;
                parts.push(attachment_part(&attachment.mime_type, bytes));
            } else {
                let in_store = store_dir.is_some_and(|dir| file.starts_with(dir));
                let location = if in_store {
                    // Outside every workspace: give the agent the absolute
                    // path (readable by read_file under the default sandbox,
                    // and by bash under containment).
                    format!("stored at \"{}\"", file.display())
                } else {
                    format!("at workspace path \"{path}\"")
                };
                // Sniff like the file-preview path does: a NUL byte in the
                // first 8 KiB means read_file will refuse to display it, so
                // the note must not send the agent down that path.
                let binary = attachment_looks_binary(&file);
                let extension = path
                    .rsplit('.')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let instruction = if binary {
                    let docx_hint = if extension == "docx" {
                        " (for .docx: unzip -p <path> word/document.xml, or pandoc/python-docx if available)"
                    } else {
                        ""
                    };
                    format!(
                        "read_file cannot display binary content; inspect it with bash{docx_hint}."
                    )
                } else {
                    "Use read_file to inspect it.".to_string()
                };
                parts.push(protocol::ContentPart::Text {
                    text: format!(
                        "[Attached file \"{}\" ({}) {location}. {instruction}]",
                        attachment.name,
                        format_attachment_size(metadata.len()),
                    ),
                });
            }
        }
    }
    Ok(parts)
}

/// Whether a file's leading bytes mark it as binary (a NUL byte within the
/// first 8 KiB — the same heuristic the file-preview path uses).
fn attachment_looks_binary(file: &std::path::Path) -> bool {
    use std::io::Read as _;
    let Ok(mut handle) = std::fs::File::open(file) else {
        return false;
    };
    let mut buffer = [0u8; 8192];
    match handle.read(&mut buffer) {
        Ok(n) => buffer[..n].contains(&0),
        Err(_) => false,
    }
}

/// Human-readable byte size for attachment notes ("5 B", "12.3 KB", "1.2 MB").
fn format_attachment_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    }
}

/// Resolve `path` inside `root`, applying the configured containment level.
fn resolve_workspace_path(
    root: &str,
    path: &str,
    enforcement: PathEnforcement,
) -> Result<PathBuf, RequestError> {
    match enforcement {
        PathEnforcement::None => Ok(Path::new(root).join(path)),
        _ => {
            let file = gewu_tools::fs_utils::resolve_safe_path(root, path)
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
            Ok(canonical_file)
        }
    }
}

/// Handle one parsed client request, producing a reply or an ask to run.
///
/// An `interrupt` request aborts and awaits the running ask task here and
/// repairs the conversation history, guaranteeing the agent lock is
/// released before any subsequent request is handled.
async fn handle_request(
    state: &Arc<ServerState>,
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
    // File operations are session-aware too: a client may inspect a persisted
    // session's selected workspace without first sending `usage` or `ask`.
    if matches!(
        &request.op,
        Op::ListFiles { .. } | Op::ReadFile { .. } | Op::WriteFile { .. }
    ) && (session.is_some() || request.session_id.is_some())
    {
        state
            .get_or_create_session(session, request.session_id.clone(), request.agent.clone())
            .await
            .map_err(|e| RequestError::Failed(e.to_string()))?;
    }
    match request.op {
        Op::Plugin(action) => {
            let current = state
                .get_or_create_session_with_workspace(
                    session,
                    request.session_id,
                    request.agent,
                    request.workspace,
                )
                .await
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            if let gewu_protocol::plugin::HostAction::AgentResult { result_id } = &action {
                use gewu_core::traits::SessionStateStore;
                let id = uuid::Uuid::parse_str(result_id)
                    .map_err(|_| RequestError::Invalid("invalid plugin result id".into()))?;
                let storage =
                    gewu_storage::SqliteSessionStateStore::new(state.bootstrap.pool.clone());
                let result = storage
                    .load(&current.session_id, &format!("gewu.plugin.result.{id}"))
                    .await
                    .map_err(|e| RequestError::Failed(e.to_string()))?
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                    .filter(|value| value["workspaceId"] == current.workspace.id)
                    .ok_or_else(|| {
                        RequestError::Failed(
                            "recorded result not found in this workspace and session".into(),
                        )
                    })?;
                return Ok(Handled::Reply(ServerMessage::Plugin {
                    id: request.id,
                    result,
                }));
            }
            let mark_session = matches!(
                &action,
                gewu_protocol::plugin::HostAction::Open { .. }
                    | gewu_protocol::plugin::HostAction::SaveText { .. }
            );
            let result = state
                .bootstrap
                .plugins
                .action(
                    &current.workspace.id,
                    Path::new(&current.workspace.path),
                    &current.session_id,
                    action,
                )
                .await
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            if mark_session {
                use gewu_core::traits::SessionStateStore;
                let store =
                    gewu_storage::SqliteSessionStateStore::new(state.bootstrap.pool.clone());
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if let Err(error) = store
                    .save(
                        &current.session_id,
                        "gewu.plugin.session",
                        timestamp.to_string(),
                    )
                    .await
                {
                    tracing::warn!("could not register plugin session for restore: {error}");
                }
            }
            Ok(Handled::Reply(ServerMessage::Plugin {
                id: request.id,
                result,
            }))
        }
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
                    let reasoning_effort = live.reasoning_effort.clone();
                    live.publish(ServerMessage::SessionSnapshot {
                        session_id: current.session_id.clone(),
                        messages: messages.clone(),
                        frames: vec![],
                        busy: false,
                        seq,
                        model: Some(model),
                        reasoning_effort,
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
                working_directory: std::path::PathBuf::from(&current.workspace.path),
                additional_tools: state.bootstrap.mcp_tools.clone(),
                skills: state.bootstrap.skills.clone(),
                enforcement: state.bootstrap.enforcement,
                image_limits: state.bootstrap.image_limits,
                context_limit: Some(current.context_limit),
                agents: state.bootstrap.agents.clone(),
                workflow_progress: Some(state.workflow_progress.clone()),
                question_broker: Some(state.questions.clone()),
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
                        let policy = registry::profile_tool_policy(
                            &target,
                            &state.bootstrap.agents,
                            &state.bootstrap.mcp_tools,
                        );
                        agent.set_profile(prompt, tools, reminder, policy);
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
                            reasoning_effort: current.reasoning_effort.clone(),
                            context_limit: current.context_limit,
                            tools: tools.clone(),
                            inbox: current.inbox.clone(),
                            workspace: current.workspace.clone(),
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
            state.release_session(&current.session_id).await;
            result
        }
        Op::SwitchWorkspace { workspace: target } => {
            if running.is_some() {
                return Err(RequestError::Failed(
                    "cannot switch workspace while a turn is running".into(),
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
            let result = match state.workspace(&target) {
                Ok(workspace) => {
                    if let Err(error) = gewu_storage::set_session_workspace(
                        &state.bootstrap.pool,
                        &current.session_id,
                        &workspace.id,
                    )
                    .await
                    {
                        Err(RequestError::Failed(error.to_string()))
                    } else {
                        current
                            .agent
                            .lock()
                            .await
                            .set_working_directory(std::path::PathBuf::from(&workspace.path));
                        let replacement = ConnectionSession {
                            workspace: workspace.clone(),
                            ..current.clone()
                        };
                        *session = Some(replacement.clone());
                        state
                            .sessions
                            .lock()
                            .await
                            .insert(current.session_id.clone(), replacement);
                        Ok(Handled::Reply(ServerMessage::WorkspaceSwitched {
                            id: request.id,
                            session_id: current.session_id.clone(),
                            workspace,
                        }))
                    }
                }
                Err(error) => Err(RequestError::Invalid(error.to_string())),
            };
            state.release_session(&current.session_id).await;
            result
        }
        Op::BrowseWorkspace { path } => {
            let (path, parent, entries) = state
                .browse_workspace(&path)
                .map_err(|e| RequestError::Invalid(e.to_string()))?;
            Ok(Handled::Reply(ServerMessage::Directories {
                id: request.id,
                path,
                parent,
                entries,
            }))
        }
        Op::AddWorkspace { path } => {
            if running.is_some() {
                return Err(RequestError::Failed(
                    "cannot add a workspace while a turn is running".into(),
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
            let result = match state.workspace_for_path(path) {
                Ok(workspace) => {
                    if let Err(error) = gewu_storage::set_session_workspace(
                        &state.bootstrap.pool,
                        &current.session_id,
                        &workspace.id,
                    )
                    .await
                    {
                        Err(RequestError::Failed(error.to_string()))
                    } else {
                        current
                            .agent
                            .lock()
                            .await
                            .set_working_directory(std::path::PathBuf::from(&workspace.path));
                        let replacement = ConnectionSession {
                            workspace: workspace.clone(),
                            ..current.clone()
                        };
                        *session = Some(replacement.clone());
                        state
                            .sessions
                            .lock()
                            .await
                            .insert(current.session_id.clone(), replacement);
                        Ok(Handled::Reply(ServerMessage::WorkspaceAdded {
                            id: request.id,
                            session_id: current.session_id.clone(),
                            workspace,
                        }))
                    }
                }
                Err(error) => Err(RequestError::Invalid(error.to_string())),
            };
            state.release_session(&current.session_id).await;
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
                Some(entry) => {
                    // Keep the explicit effort only when the new model
                    // supports it; otherwise fall back to its default.
                    let reasoning_effort = current
                        .reasoning_effort
                        .clone()
                        .filter(|effort| entry.supports_effort(effort));
                    let Some(provider) = entry.provider_for(reasoning_effort.as_deref()) else {
                        state.release_session(&current.session_id).await;
                        return Err(RequestError::Invalid(format!(
                            "reasoning effort is unavailable for model '{target}'"
                        )));
                    };
                    match gewu_storage::set_session_model(
                        &state.bootstrap.pool,
                        &current.session_id,
                        &target,
                    )
                    .await
                    {
                        Ok(()) => match gewu_storage::set_session_reasoning_effort(
                            &state.bootstrap.pool,
                            &current.session_id,
                            reasoning_effort.as_deref(),
                        )
                        .await
                        {
                            Ok(()) => {
                                {
                                    let mut agent = current.agent.lock().await;
                                    agent.set_llm(provider.clone());
                                    agent.context_limit = entry.context_limit;
                                }
                                let replacement = ConnectionSession {
                                    agent: current.agent.clone(),
                                    session_id: current.session_id.clone(),
                                    agent_name: current.agent_name.clone(),
                                    model: target.clone(),
                                    provider,
                                    reasoning_effort: reasoning_effort.clone(),
                                    context_limit: entry.context_limit,
                                    tools: current.tools.clone(),
                                    inbox: current.inbox.clone(),
                                    workspace: current.workspace.clone(),
                                };
                                *session = Some(replacement.clone());
                                state
                                    .sessions
                                    .lock()
                                    .await
                                    .insert(current.session_id.clone(), replacement);
                                if let Some(turn) =
                                    state.turns.lock().await.get(&current.session_id).cloned()
                                {
                                    let mut turn = turn.lock().await;
                                    turn.model = target.clone();
                                    turn.reasoning_effort = reasoning_effort.clone();
                                }
                                Ok(Handled::Reply(ServerMessage::ModelSwitched {
                                    id: request.id,
                                    session_id: current.session_id.clone(),
                                    model: target,
                                    label: entry.label.clone(),
                                    reasoning_effort,
                                }))
                            }
                            Err(e) => Err(RequestError::Failed(e.to_string())),
                        },
                        Err(e) => Err(RequestError::Failed(e.to_string())),
                    }
                }
                None => Err(RequestError::Invalid(format!("unknown model: {target}"))),
            };
            state.release_session(&current.session_id).await;
            result
        }
        Op::SwitchReasoningEffort { effort } => {
            if running.is_some() {
                return Err(RequestError::Failed(
                    "cannot switch reasoning effort while a turn is running".into(),
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
            let result = match state.bootstrap.models.get(&current.model).cloned() {
                Some(entry) if !entry.supports_reasoning_effort => {
                    Err(RequestError::Invalid(format!(
                        "model '{}' does not support reasoning effort",
                        current.model
                    )))
                }
                Some(entry) => {
                    if let Some(effort) = &effort
                        && !entry.supports_effort(effort)
                    {
                        Err(RequestError::Invalid(format!(
                            "unknown reasoning effort: {effort}"
                        )))
                    } else {
                        match entry.provider_for(effort.as_deref()) {
                            Some(provider) => match gewu_storage::set_session_reasoning_effort(
                                &state.bootstrap.pool,
                                &current.session_id,
                                effort.as_deref(),
                            )
                            .await
                            {
                                Ok(()) => {
                                    current.agent.lock().await.set_llm(provider.clone());
                                    let replacement = ConnectionSession {
                                        agent: current.agent.clone(),
                                        session_id: current.session_id.clone(),
                                        agent_name: current.agent_name.clone(),
                                        model: current.model.clone(),
                                        provider,
                                        reasoning_effort: effort.clone(),
                                        context_limit: current.context_limit,
                                        tools: current.tools.clone(),
                                        inbox: current.inbox.clone(),
                                        workspace: current.workspace.clone(),
                                    };
                                    *session = Some(replacement.clone());
                                    state
                                        .sessions
                                        .lock()
                                        .await
                                        .insert(current.session_id.clone(), replacement);
                                    if let Some(turn) =
                                        state.turns.lock().await.get(&current.session_id).cloned()
                                    {
                                        turn.lock().await.reasoning_effort = effort.clone();
                                    }
                                    Ok(Handled::Reply(ServerMessage::ReasoningEffortSwitched {
                                        id: request.id,
                                        session_id: current.session_id.clone(),
                                        effort,
                                    }))
                                }
                                Err(e) => Err(RequestError::Failed(e.to_string())),
                            },
                            None => Err(RequestError::Invalid(format!(
                                "reasoning effort '{}' is unavailable for model '{}'",
                                effort.as_deref().unwrap_or("default"),
                                current.model
                            ))),
                        }
                    }
                }
                None => Err(RequestError::Failed(format!(
                    "unknown model: {}",
                    current.model
                ))),
            };
            state.release_session(&current.session_id).await;
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
                    supports_reasoning_effort: entry.supports_reasoning_effort,
                    default_effort: entry.default_effort.clone(),
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

        Op::Info => {
            let active = if request.session_id.is_some() || session.is_some() {
                state
                    .get_or_create_session(session, request.session_id, request.agent)
                    .await
                    .map_err(|e| RequestError::Failed(e.to_string()))?
                    .workspace
            } else {
                state
                    .workspace("cwd")
                    .map_err(|e| RequestError::Failed(e.to_string()))?
            };
            let mut workspaces = state.workspace_summaries();
            if !workspaces.iter().any(|workspace| workspace.id == active.id) {
                workspaces.push(active.clone());
            }
            Ok(Handled::Reply(ServerMessage::Info {
                id: request.id,
                model: state.bootstrap.model.clone(),
                provider: state.bootstrap.provider_name.clone(),
                working_directory: active.path.clone(),
                workspaces,
                active_workspace: active.id,
            }))
        }

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
            let root = session
                .as_ref()
                .map(|current| current.workspace.path.clone())
                .unwrap_or_else(|| state.bootstrap.working_directory.clone());
            let dir = match state.bootstrap.enforcement {
                PathEnforcement::None => {
                    let dir = std::path::Path::new(&root).join(&path);
                    if !dir.is_dir() {
                        return Err(RequestError::Invalid("path is not a directory".into()));
                    }
                    dir
                }
                _ => {
                    let dir = gewu_tools::fs_utils::resolve_safe_path(&root, &path)
                        .map_err(|e| RequestError::Invalid(e.to_string()))?;
                    let canonical_root = std::fs::canonicalize(&root).map_err(|e| {
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
            // Only dotfiles are hidden here; ignore rules (`.gitignore`,
            // `.ignore`, git excludes) must not filter the file tree.
            for entry in ignore::WalkBuilder::new(&dir)
                .max_depth(Some(1))
                .hidden(true)
                .git_ignore(false)
                .git_global(false)
                .git_exclude(false)
                .ignore(false)
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
            let root = session
                .as_ref()
                .map(|current| current.workspace.path.clone())
                .unwrap_or_else(|| state.bootstrap.working_directory.clone());
            let file = resolve_workspace_path(&root, &path, state.bootstrap.enforcement)?;
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
            let root = session
                .as_ref()
                .map(|current| current.workspace.path.clone())
                .unwrap_or_else(|| state.bootstrap.working_directory.clone());
            let canonical_root = std::fs::canonicalize(&root).map_err(|e| {
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

        Op::UploadAttachment {
            name,
            content_base64,
        } => {
            const MAX_ATTACHMENT_UPLOAD_BYTES: usize = 50 * 1024 * 1024;
            // Only a bare file name is accepted; the store is per-session, so
            // a same-name re-attach replaces the earlier file instead of
            // colliding across sessions or workspace files.
            let name = std::path::Path::new(&name)
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty() && *name != "." && *name != "..")
                .ok_or_else(|| RequestError::Invalid("name must be a file name".into()))?;
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(content_base64.as_bytes())
                .map_err(|e| {
                    RequestError::Invalid(format!("content_base64 is not valid base64: {e}"))
                })?;
            if bytes.len() > MAX_ATTACHMENT_UPLOAD_BYTES {
                return Err(RequestError::Invalid(format!(
                    "attachment exceeds the {} MB limit",
                    MAX_ATTACHMENT_UPLOAD_BYTES / (1024 * 1024)
                )));
            }
            let session = session.as_ref().ok_or_else(|| {
                RequestError::Failed("attachments require an active session".into())
            })?;
            let store_dir = state.bootstrap.attachment_store.join(&session.session_id);
            std::fs::create_dir_all(&store_dir).map_err(|e| {
                RequestError::Failed(format!("cannot create attachment store: {e}"))
            })?;
            let target = store_dir.join(name);
            std::fs::write(&target, &bytes)
                .map_err(|e| RequestError::Failed(format!("cannot store attachment: {e}")))?;
            Ok(Handled::Reply(ServerMessage::AttachmentStored {
                id: request.id,
                name: name.to_string(),
                path: target.to_string_lossy().to_string(),
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
                if let Some(session) = state.sessions.lock().await.remove(&session_id) {
                    session.inbox.stop();
                }
                state.turns.lock().await.remove(&session_id);
                state.workflows.lock().await.remove(&session_id);
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

        Op::QuestionReply {
            request_id,
            answers,
        } => {
            let session_id = request
                .session_id
                .or_else(|| session.as_ref().map(|s| s.session_id.clone()))
                .or_else(|| state.questions.session_of(&request_id))
                .ok_or_else(|| {
                    RequestError::Failed("no session for this question reply".to_string())
                })?;
            let answers = state
                .questions
                .reply(&session_id, &request_id, answers)
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            Ok(Handled::Reply(ServerMessage::QuestionReplied {
                id: request.id,
                session_id,
                request_id,
                answers: Some(answers),
            }))
        }

        Op::QuestionReject { request_id } => {
            let session_id = request
                .session_id
                .or_else(|| session.as_ref().map(|s| s.session_id.clone()))
                .or_else(|| state.questions.session_of(&request_id))
                .ok_or_else(|| {
                    RequestError::Failed("no session for this question rejection".to_string())
                })?;
            state
                .questions
                .reject(&session_id, &request_id)
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            Ok(Handled::Reply(ServerMessage::QuestionReplied {
                id: request.id,
                session_id,
                request_id,
                answers: None,
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
            let sessions = state.sessions.lock().await;
            let Some(session) = sessions.get(&target) else {
                return Err(RequestError::Failed(format!("unknown session '{target}'")));
            };
            session
                .inbox
                .steering
                .send(text.clone())
                .map_err(|e| RequestError::Failed(format!("steering failed: {e}")))?;
            Ok(Handled::Reply(ServerMessage::Steered {
                id: request.id,
                session_id: target,
                text,
            }))
        }

        Op::Ask { prompt } => {
            if running.is_some() {
                return Err(RequestError::Failed(
                    "an ask is already in progress; send 'interrupt' first".to_string(),
                ));
            }
            let session = state
                .get_or_create_session_with_workspace(
                    session,
                    request.session_id,
                    request.agent,
                    request.workspace,
                )
                .await
                .map_err(|e| RequestError::Failed(e.to_string()))?;
            // Resolve attachments before claiming the session so a bad
            // attachment cannot strand a claim.
            let root = session.workspace.path.clone();
            let store_dir = state.bootstrap.attachment_store.join(&session.session_id);
            let media = resolve_ask_attachments(
                &root,
                state.bootstrap.enforcement,
                Some(&store_dir),
                &request.attachments,
            )?;
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
                input: AskInput { prompt, media },
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
    media: Vec<protocol::ContentPart>,
    pool: &sqlx::SqlitePool,
    mut emit: E,
) where
    E: FnMut(ServerMessage) -> Fut,
    Fut: std::future::Future<Output = Result<(), anyhow::Error>>,
{
    let mut agent = session.agent.lock().await;
    // Repair any history left inconsistent by a previous interrupt.
    patch_dangling_tool_calls(&mut agent).await;

    let mut client_gone = false;
    let mut terminal_sent = false;
    {
        let mut stream: std::pin::Pin<
            Box<dyn futures::Stream<Item = anyhow::Result<AgentEvent>> + Send + '_>,
        > = if prompt.is_empty() && media.is_empty() {
            Box::pin(agent.resume_with_inbox(session.inbox.steering.clone()))
        } else {
            Box::pin(agent.ask_parts_with_inbox(prompt, media, session.inbox.steering.clone()))
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
    mut input: AskInput,
    ready: ServerMessage,
    subscriber: Option<Subscriber>,
    handoff_ordinary: bool,
) {
    let (mut messages, original_tools) = {
        let mut agent = session.agent.lock().await;
        let plugin_tools = if session.agent_name == "build" {
            state
                .bootstrap
                .plugins
                .tools(
                    &session.workspace.id,
                    PathBuf::from(&session.workspace.path),
                    &session.session_id,
                    agent.tools(),
                )
                .await
        } else {
            Vec::new()
        };
        let previous = if plugin_tools.is_empty() {
            None
        } else {
            let mut tools = agent.tools().to_vec();
            tools.extend(plugin_tools);
            Some(agent.replace_tools(Arc::new(tools)))
        };
        (agent.memory.messages().to_vec(), previous)
    };
    let AskInput { prompt, media } = &mut input;
    if !prompt.is_empty() || !media.is_empty() {
        session.inbox.steering.unpause();
        let mut content = Vec::with_capacity(media.len() + 1);
        if !prompt.is_empty() {
            let selected = state
                .bootstrap
                .plugins
                .selected_context(&session.workspace.id, &session.session_id);
            if !selected.is_empty() {
                prompt.push_str("\n\n[Plugin selection context: untrusted view data]\n");
                prompt.push_str(&serde_json::to_string(&selected).unwrap());
            }
            content.push(protocol::ContentPart::Text {
                text: prompt.clone(),
            });
        }
        content.extend(media.iter().cloned());
        messages.push(Message::User { content });
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
        reasoning_effort: session.reasoning_effort.clone(),
        done: Arc::new(tokio::sync::Notify::new()),
        subscribers: vec![],
        cancel: Some(cancel),
        interrupted: false,
        terminal: None,
    }));
    let previous = state.turns.lock().await.get(&session.session_id).cloned();
    let previous = match previous {
        Some(previous) => Some(previous.lock_owned().await),
        None => None,
    };
    let mut new_turn = live.lock().await;
    // Register execution before acknowledging it: a failed socket write does
    // not strand a claimed session without its worker.
    state
        .turns
        .lock()
        .await
        .insert(session.session_id.clone(), live.clone());
    if let Some(mut previous) = previous {
        let snapshot = ServerMessage::SessionSnapshot {
            session_id: session.session_id.clone(),
            messages: new_turn.messages.clone(),
            frames: vec![],
            busy: true,
            seq: 0,
            model: Some(session.model.clone()),
            reasoning_effort: session.reasoning_effort.clone(),
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
                new_turn.subscribers.push(observer);
            }
        }
    }
    if let Some(subscriber) = subscriber
        && subscriber.send(&ready.to_line())
    {
        new_turn.subscribers.push(subscriber);
    }
    drop(new_turn);
    let worker_pool = state.bootstrap.pool.clone();
    tokio::spawn(async move {
        let worker_session = session.clone();
        let worker_live = live.clone();
        let plugin_host = state.bootstrap.plugins.clone();
        let mut worker = tokio::spawn(async move {
            execute_ask(
                &worker_session,
                &request_id,
                &input.prompt,
                input.media,
                &worker_pool,
                |message| {
                    let live = worker_live.clone();
                    if let ServerMessage::Event { event, .. } = &message
                        && event["type"] == "tool_finish"
                        && let Ok(event) = serde_json::from_value::<AgentEvent>(event.clone())
                    {
                        plugin_host.record_agent_changes(
                            &worker_session.workspace.id,
                            &worker_session.session_id,
                            &event,
                        );
                    }
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
            if let Some(tools) = original_tools {
                agent.replace_tools(tools);
            }
            patch_dangling_tool_calls(&mut agent).await;

            if failure.is_some() {
                drop_dangling_prompt(&mut agent).await;
                // The aborted tool tasks are dropped asynchronously; remove
                // their pending questions now so the session leaves no stale
                // requests behind once the interrupt returns.
                state.questions.cancel_session(&session.session_id);
            }
            let mut live = live.lock().await;
            if failure.is_some() && !live.interrupted {
                session.inbox.steering.pause();
            }
            let terminal = failure
                .map(|error| ServerMessage::Error { id: None, error })
                .or_else(|| live.terminal.take());
            live.messages = agent.memory.messages().to_vec();
            live.frames.clear();
            live.bytes = 0;
            let mut busy_sessions = state.busy_sessions.lock().await;
            busy_sessions.remove(&session.session_id);
            live.busy = false;
            live.cancel = None;
            // Wake any interrupt awaiting finalization (see `cancel_turn`).
            live.done.notify_waiters();
            if let Some(terminal) = terminal {
                live.publish(terminal);
            }
            live.frames.clear();
            live.bytes = 0;
            let pending = session.inbox.steering.should_resume();
            live.subscribers
                .retain(|s| (s.persistent || pending) && !s.out.is_closed());
            drop(busy_sessions);
            session.inbox.steering.wake();
        }
    });
}

async fn watch_inbox(
    state: Weak<ServerState>,
    session_id: String,
    owner: Weak<SessionInbox>,
    inbox: gewu_core::Steering,
) {
    loop {
        inbox.notified().await;
        let Some(state) = state.upgrade() else { return };
        if !inbox.should_resume() {
            continue;
        }
        let _lifecycle = state.lifecycle.lock().await;
        let Some(session) = state.sessions.lock().await.get(&session_id).cloned() else {
            return;
        };
        if owner.as_ptr() != Arc::as_ptr(&session.inbox) {
            return;
        }
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
        let ready = ServerMessage::Ready {
            id: Value::Null,
            request_id: String::new(),
            session_id: session.session_id.clone(),
            agent: session.agent_name.clone(),
        };
        start_session_turn(
            state.clone(),
            session,
            crate::bootstrap::new_session_id(),
            AskInput {
                prompt: String::new(),
                media: Vec::new(),
            },
            ready,
            None,
            true,
        )
        .await;
    }
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
                    Ok(Handled::Ask { ready, request_id, session_id, input }) => {
                        let session = session
                            .clone()
                            .expect("session created by handle_request");
                        start_session_turn(state.clone(), session, request_id, input, ready,
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[cfg(unix)]
    mod plugins;

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
        /// Ask the user a `question`, then (second turn) finish with text.
        QuestionCall,
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
        fn question_call() -> Arc<Self> {
            Arc::new(Self {
                behavior: LlmBehavior::QuestionCall,
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
                LlmBehavior::QuestionCall if call == 1 => vec![
                    gewu_core::types::ProviderEvent::ToolCallStart {
                        id: "call-q".to_string(),
                        name: "question".to_string(),
                    },
                    gewu_core::types::ProviderEvent::ToolCallDelta {
                        id: "call-q".to_string(),
                        delta: serde_json::json!({
                            "questions": [{
                                "question": "Which database?",
                                "header": "Database",
                                "options": [
                                    { "label": "Postgres", "description": "Relational" },
                                    { "label": "SQLite", "description": "Embedded" }
                                ]
                            }]
                        })
                        .to_string(),
                    },
                    gewu_core::types::ProviderEvent::Finish {
                        reason: Some("tool_calls".to_string()),
                    },
                ],
                LlmBehavior::QuestionCall => vec![
                    gewu_core::types::ProviderEvent::ContentDelta {
                        delta: "Question answered".to_string(),
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

    /// LLM that records the user prompts of every provider request and can
    /// hold the first response so tests observe a turn mid-flight. Used to
    /// observe exactly what history reaches the model.
    struct RecordingLlm {
        /// User prompts per provider request, in build order.
        requests: std::sync::Mutex<Vec<Vec<String>>>,
        first_send: tokio::sync::Notify,
        release_first: tokio::sync::Notify,
        first_send_seen: AtomicBool,
    }

    impl RecordingLlm {
        fn gated() -> Arc<Self> {
            Arc::new(Self {
                requests: std::sync::Mutex::new(Vec::new()),
                first_send: tokio::sync::Notify::new(),
                release_first: tokio::sync::Notify::new(),
                first_send_seen: AtomicBool::new(false),
            })
        }

        fn requests(&self) -> Vec<Vec<String>> {
            self.requests.lock().unwrap().clone()
        }

        async fn wait_for_first_send(&self) {
            self.first_send.notified().await;
        }

        fn release_first_send(&self) {
            self.release_first.notify_one();
        }
    }

    #[async_trait::async_trait]
    impl gewu_core::traits::LlmProvider for RecordingLlm {
        fn build(
            &self,
            messages: &[Message],
            _tools: &[Arc<dyn gewu_core::traits::Tool>],
        ) -> std::result::Result<Value, Box<dyn Error + Send + Sync>> {
            let mut request = Vec::new();
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
                        request.push(text);
                    }
                }
            }
            if !request.is_empty() {
                self.requests.lock().unwrap().push(request);
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
            if !self.first_send_seen.swap(true, Ordering::SeqCst) {
                self.first_send.notify_one();
                self.release_first.notified().await;
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
            reasoning_effort: None,
            context_limit: 256_000,
            tools,
            inbox: Arc::new(SessionInbox::default()),
            workspace: WorkspaceSummary {
                id: "cwd".into(),
                path: "/tmp/gewu-test".into(),
            },
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
            reasoning_effort: None,
            context_limit: 256_000,
            tools,
            inbox: Arc::new(SessionInbox::default()),
            workspace: WorkspaceSummary {
                id: "cwd".into(),
                path: "/tmp/gewu-test".into(),
            },
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
                execute_ask(&session, "req-test", &prompt, Vec::new(), &pool, |msg| {
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
        tokio::task::JoinHandle<Result<()>>,
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
    async fn subscribe_replays_live_workflow_progress() {
        let state = server_state_with(TestLlm::plain());
        let run = WorkflowRunState {
            run_id: "r-1".into(),
            name: Some("docs".into()),
            status: "running".into(),
            current_node: Some("read".into()),
            tasks_done: 1,
            tasks_failed: 0,
            tasks_attempted: 1,
            nodes: vec![
                WorkflowNodeState {
                    id: "read".into(),
                    status: "ok".into(),
                    done: 1,
                    failed: 0,
                },
                WorkflowNodeState {
                    id: "write".into(),
                    status: "pending".into(),
                    done: 0,
                    failed: 0,
                },
            ],
            error: None,
        };
        state
            .workflows
            .lock()
            .await
            .insert("observed".into(), BTreeMap::from([("r-1".into(), run)]));

        let (observer, mut updates, observer_task) = spawn_connection(state.clone()).await;
        observer
            .send(ClientRequest::subscribe_session(1, "observed").to_line())
            .await
            .unwrap();
        assert_eq!(recv_json(&mut updates).await["type"], "ready");
        assert_eq!(recv_json(&mut updates).await["type"], "session_snapshot");
        let progress = recv_json(&mut updates).await;
        assert_eq!(progress["type"], "workflow_progress");
        assert_eq!(progress["session_id"], "observed");
        assert_eq!(progress["run"]["run_id"], "r-1");
        assert_eq!(progress["run"]["status"], "running");
        assert_eq!(progress["run"]["nodes"][0]["status"], "ok");
        assert_eq!(progress["run"]["nodes"][1]["status"], "pending");

        drop(observer);
        drop(updates);
        observer_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn workflow_progress_is_forwarded_only_to_the_owning_session() {
        use gewu_tools::workflow::RunProgress;
        use gewu_tools::workflow::engine::RunStatus;
        use gewu_tools::workflow::manager::NodeReport;

        let state = server_state_with(TestLlm::plain());
        let (observer, mut updates, observer_task) = spawn_connection(state.clone()).await;
        observer
            .send(ClientRequest::subscribe_session(1, "owner").to_line())
            .await
            .unwrap();
        assert_eq!(recv_json(&mut updates).await["type"], "ready");
        assert_eq!(recv_json(&mut updates).await["type"], "session_snapshot");

        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tokio::spawn(forward_workflow_progress(state.clone(), rx));
        let progress = |session: &str| RunProgress {
            owner_session: session.into(),
            run_id: "r-1".into(),
            name: Some("docs".into()),
            status: RunStatus::Running,
            current_node: Some("read".into()),
            tasks_done: 0,
            tasks_failed: 0,
            tasks_attempted: 0,
            nodes: vec![NodeReport {
                id: "read".into(),
                status: "pending".into(),
                done: 0,
                failed: Vec::new(),
                sessions: Vec::new(),
            }],
            error: None,
        };
        // Another session's progress must not reach this observer.
        tx.send(progress("other")).unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(200),
                recv_json(&mut updates)
            )
            .await
            .is_err()
        );

        tx.send(progress("owner")).unwrap();
        let message = recv_json(&mut updates).await;
        assert_eq!(message["type"], "workflow_progress");
        assert_eq!(message["run"]["run_id"], "r-1");
        assert_eq!(message["run"]["nodes"][0]["status"], "pending");
        assert!(
            state
                .workflows
                .lock()
                .await
                .get("owner")
                .is_some_and(|runs| runs.contains_key("r-1"))
        );

        drop(tx);
        drop(observer);
        drop(updates);
        observer_task.await.unwrap().unwrap();
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
                plugins: Default::default(),
                skills: Vec::new(),
                log_writer: None,
                model: "test-model".to_string(),
                provider_name: "test".to_string(),
                working_directory: "/tmp/gewu-test".to_string(),
                workspace_roots: Arc::new(BTreeMap::from([(
                    "cwd".to_string(),
                    std::path::PathBuf::from("/tmp/gewu-test"),
                )])),
                enforcement: PathEnforcement::None,
                image_limits: gewu_tools::ImageLimits::default(),
                context_limit: None,
                models: BTreeMap::from([
                    (
                        "test-model".to_string(),
                        crate::bootstrap::ModelEntry {
                            provider: provider.clone(),
                            provider_name: "test".to_string(),
                            label: "test-model".to_string(),
                            context_limit: 256_000,
                            default_effort: Some("high".to_string()),
                            effort_providers: protocol::REASONING_EFFORTS
                                .iter()
                                .map(|effort| (effort.to_string(), provider.clone()))
                                .chain(std::iter::once((
                                    protocol::PROVIDER_DEFAULT_EFFORT.to_string(),
                                    provider.clone(),
                                )))
                                .collect(),
                            supports_reasoning_effort: true,
                        },
                    ),
                    (
                        "test-plain".to_string(),
                        crate::bootstrap::ModelEntry {
                            provider: provider.clone(),
                            provider_name: "test".to_string(),
                            label: "test-plain".to_string(),
                            context_limit: 256_000,
                            default_effort: None,
                            effort_providers: BTreeMap::new(),
                            supports_reasoning_effort: false,
                        },
                    ),
                ]),
                model_selector: "test-model".to_string(),
                agents: Arc::new(BTreeMap::new()),
                attachment_store: std::env::temp_dir().join("gewu-test-attachments"),
            },
            token: token.map(str::to_string),
            busy_sessions: Mutex::new(HashSet::new()),
            sessions: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            workflow_progress: tokio::sync::broadcast::channel(16).0,
            workflows: Mutex::new(HashMap::new()),
            questions: Arc::new(gewu_core::QuestionBroker::new()),
            lifecycle: Mutex::new(()),
        })
    }

    #[tokio::test]
    async fn reasoning_effort_switch_persists_and_reconnects() {
        let state = server_state_with(TestLlm::plain());
        let (input, mut output, _connection) = spawn_connection(state.clone()).await;

        input
            .send(r#"{"id":1,"op":"list_models"}"#.into())
            .await
            .unwrap();
        let models = recv_json(&mut output).await;
        assert_eq!(models["models"][0]["supports_reasoning_effort"], true);
        assert_eq!(models["models"][0]["default_effort"], "high");

        input
            .send(
                r#"{"id":2,"op":"switch_reasoning_effort","session_id":"eff","effort":"low"}"#
                    .into(),
            )
            .await
            .unwrap();
        let reply = recv_json(&mut output).await;
        assert_eq!(reply["type"], "reasoning_effort_switched");
        assert_eq!(reply["effort"], "low");
        assert_eq!(
            gewu_storage::session_reasoning_effort(&state.bootstrap.pool, "eff")
                .await
                .unwrap()
                .as_deref(),
            Some("low")
        );
        let session = state.sessions.lock().await.get("eff").unwrap().clone();
        assert_eq!(session.reasoning_effort.as_deref(), Some("low"));

        input
            .send(ClientRequest::subscribe_session(3, "eff").to_line())
            .await
            .unwrap();
        assert_eq!(recv_json(&mut output).await["type"], "ready");
        let snapshot = recv_json(&mut output).await;
        assert_eq!(snapshot["type"], "session_snapshot");
        assert_eq!(snapshot["reasoning_effort"], "low");

        // The provider default is its own explicit, durable selection.
        input
            .send(r#"{"id":8,"op":"switch_reasoning_effort","effort":"provider-default"}"#.into())
            .await
            .unwrap();
        let reply = recv_json(&mut output).await;
        assert_eq!(reply["type"], "reasoning_effort_switched");
        assert_eq!(reply["effort"], "provider-default");
        assert_eq!(
            gewu_storage::session_reasoning_effort(&state.bootstrap.pool, "eff")
                .await
                .unwrap()
                .as_deref(),
            Some("provider-default")
        );
        let session = state.sessions.lock().await.get("eff").unwrap().clone();
        assert_eq!(
            session.reasoning_effort.as_deref(),
            Some("provider-default")
        );

        input
            .send(ClientRequest::subscribe_session(9, "eff").to_line())
            .await
            .unwrap();
        assert_eq!(recv_json(&mut output).await["type"], "ready");
        let snapshot = recv_json(&mut output).await;
        assert_eq!(snapshot["reasoning_effort"], "provider-default");

        // A null effort clears the explicit selection (configured default).
        input
            .send(r#"{"id":4,"op":"switch_reasoning_effort","effort":null}"#.into())
            .await
            .unwrap();
        let reply = recv_json(&mut output).await;
        assert_eq!(reply["type"], "reasoning_effort_switched");
        assert_eq!(reply["effort"], Value::Null);
        assert_eq!(
            gewu_storage::session_reasoning_effort(&state.bootstrap.pool, "eff")
                .await
                .unwrap(),
            None
        );

        input
            .send(ClientRequest::subscribe_session(5, "eff").to_line())
            .await
            .unwrap();
        assert_eq!(recv_json(&mut output).await["type"], "ready");
        let snapshot = recv_json(&mut output).await;
        assert_eq!(snapshot["type"], "session_snapshot");
        assert!(snapshot.get("reasoning_effort").is_none(), "{snapshot}");

        input
            .send(r#"{"id":6,"op":"switch_reasoning_effort","effort":"bogus"}"#.into())
            .await
            .unwrap();
        let error = recv_json(&mut output).await;
        assert_eq!(error["type"], "error");
        assert!(
            error["error"]
                .as_str()
                .unwrap()
                .contains("unknown reasoning effort"),
            "{error}"
        );

        // Switching to a model without effort support clears the selection.
        input
            .send(r#"{"id":7,"op":"switch_model","model":"test-plain"}"#.into())
            .await
            .unwrap();
        let reply = recv_json(&mut output).await;
        assert_eq!(reply["type"], "model_switched");
        assert!(reply.get("reasoning_effort").is_none(), "{reply}");
        let session = state.sessions.lock().await.get("eff").unwrap().clone();
        assert_eq!(session.model, "test-plain");
        assert_eq!(session.reasoning_effort, None);
        assert_eq!(
            gewu_storage::session_reasoning_effort(&state.bootstrap.pool, "eff")
                .await
                .unwrap(),
            None
        );
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
    async fn switching_workspace_changes_file_api_root_and_persists() {
        let cwd = tempfile::tempdir().unwrap();
        let frontend = tempfile::tempdir().unwrap();
        std::fs::write(frontend.path().join("active.txt"), "frontend root").unwrap();
        let mut state = server_state_with(TestLlm::plain());
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        {
            let bootstrap = &mut Arc::get_mut(&mut state).unwrap().bootstrap;
            bootstrap.pool = pool;
            bootstrap.working_directory = cwd.path().to_string_lossy().to_string();
            bootstrap.workspace_roots = Arc::new(BTreeMap::from([
                ("cwd".into(), cwd.path().to_path_buf()),
                ("frontend".into(), frontend.path().to_path_buf()),
            ]));
        }

        let mut session = None;
        let mut running = None;
        let switched = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::switch_workspace(1, "frontend"),
        )
        .await
        .unwrap();
        assert!(matches!(
            switched,
            Handled::Reply(ServerMessage::WorkspaceSwitched { ref workspace, .. })
                if workspace.id == "frontend"
        ));

        let read = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::read_file(2, "active.txt"),
        )
        .await
        .unwrap();
        match read {
            Handled::Reply(ServerMessage::FileContent { content, .. }) => {
                assert_eq!(content, "frontend root")
            }
            _ => panic!("unexpected reply"),
        }
        assert_eq!(
            gewu_storage::session_workspace(
                &state.bootstrap.pool,
                session.as_ref().unwrap().session_id.as_str()
            )
            .await
            .unwrap()
            .as_deref(),
            Some("frontend")
        );

        let session_id = session.as_ref().unwrap().session_id.clone();
        let mut other_connection = None;
        let mut other_running = None;
        let direct = handle_request(
            &state,
            &mut other_connection,
            &mut other_running,
            ClientRequest::read_file(3, "active.txt").with_session(session_id),
        )
        .await
        .unwrap();
        assert!(matches!(
            direct,
            Handled::Reply(ServerMessage::FileContent { ref content, .. })
                if content == "frontend root"
        ));
    }

    #[tokio::test]
    async fn list_files_shows_ignored_entries_but_not_dotfiles() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join(".gitignore"), "ignored.txt\nlogs/\n").unwrap();
        std::fs::write(cwd.path().join("ignored.txt"), "ignored\n").unwrap();
        std::fs::write(cwd.path().join(".ignore"), "*.tmp\n").unwrap();
        std::fs::write(cwd.path().join("scratch.tmp"), "scratch\n").unwrap();
        std::fs::write(cwd.path().join("visible.txt"), "visible\n").unwrap();
        std::fs::write(cwd.path().join(".hidden"), "hidden\n").unwrap();
        std::fs::create_dir(cwd.path().join("logs")).unwrap();
        std::fs::create_dir(cwd.path().join(".hiddendir")).unwrap();

        let mut state = server_state_with(TestLlm::plain());
        Arc::get_mut(&mut state)
            .unwrap()
            .bootstrap
            .working_directory = cwd.path().to_string_lossy().to_string();
        let mut session = None;
        let mut running = None;
        let result = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::list_files(1, ""),
        )
        .await
        .unwrap();
        let entries = match result {
            Handled::Reply(ServerMessage::Files { entries, .. }) => entries,
            _ => panic!("expected files reply"),
        };
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(
            names,
            ["logs", "ignored.txt", "scratch.tmp", "visible.txt"],
            "{names:?}"
        );
    }

    #[tokio::test]
    async fn browsing_workspaces_hides_dot_entries_and_files() {
        let cwd = tempfile::tempdir().unwrap();
        for name in ["zebra", "Alpha", ".hidden"] {
            std::fs::create_dir(cwd.path().join(name)).unwrap();
        }
        std::fs::write(cwd.path().join("file.txt"), "test").unwrap();
        let state = server_state_with(TestLlm::plain());
        let (_, _, entries) = state
            .browse_workspace(cwd.path().to_str().unwrap())
            .unwrap();
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "zebra"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn browsing_workspaces_includes_only_visible_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let cwd = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::fs::write(target.path().join("file.txt"), "test").unwrap();
        symlink(target.path(), cwd.path().join("linked-folder")).unwrap();
        symlink(target.path(), cwd.path().join(".hidden-link")).unwrap();
        symlink(
            target.path().join("file.txt"),
            cwd.path().join("linked-file"),
        )
        .unwrap();
        symlink(target.path().join("missing"), cwd.path().join("broken")).unwrap();
        symlink("cycle", cwd.path().join("cycle")).unwrap();
        let state = server_state_with(TestLlm::plain());
        let (_, _, entries) = state
            .browse_workspace(cwd.path().to_str().unwrap())
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "linked-folder");
        assert_eq!(
            entries[0].path,
            cwd.path().join("linked-folder").to_string_lossy()
        );
        let (path, _, _) = state.browse_workspace(&entries[0].path).unwrap();
        assert_eq!(
            path,
            target.path().canonicalize().unwrap().to_string_lossy()
        );
    }

    #[tokio::test]
    async fn adding_runtime_workspace_selects_and_persists_it() {
        let cwd = tempfile::tempdir().unwrap();
        let added = tempfile::tempdir().unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let mut state = server_state_with(TestLlm::plain());
        {
            let bootstrap = &mut Arc::get_mut(&mut state).unwrap().bootstrap;
            bootstrap.pool = pool;
            bootstrap.working_directory = cwd.path().to_string_lossy().to_string();
            bootstrap.workspace_roots =
                Arc::new(BTreeMap::from([("cwd".into(), cwd.path().to_path_buf())]));
        }
        let mut session = None;
        let mut running = None;
        let result = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::add_workspace(1, added.path().to_string_lossy().to_string()),
        )
        .await
        .unwrap();
        let workspace = match result {
            Handled::Reply(ServerMessage::WorkspaceAdded { workspace, .. }) => workspace,
            _ => panic!("expected workspace_added"),
        };
        assert!(workspace.id.starts_with("runtime:"));
        assert_eq!(
            workspace.path,
            added.path().canonicalize().unwrap().to_string_lossy()
        );
        assert_eq!(
            gewu_storage::session_workspace(
                &state.bootstrap.pool,
                &session.as_ref().unwrap().session_id,
            )
            .await
            .unwrap()
            .as_deref(),
            Some(workspace.id.as_str())
        );
    }

    #[tokio::test]
    async fn first_ask_workspace_is_used_when_creating_a_session() {
        let cwd = tempfile::tempdir().unwrap();
        let tachyon = tempfile::tempdir().unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let mut state = server_state_with(TestLlm::plain());
        {
            let bootstrap = &mut Arc::get_mut(&mut state).unwrap().bootstrap;
            bootstrap.pool = pool;
            bootstrap.working_directory = cwd.path().to_string_lossy().to_string();
            bootstrap.workspace_roots = Arc::new(BTreeMap::from([
                ("cwd".into(), cwd.path().to_path_buf()),
                ("tachyon".into(), tachyon.path().to_path_buf()),
            ]));
        }
        let mut session = None;
        let mut running = None;
        let result = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::ask(1, "pwd").with_workspace("tachyon"),
        )
        .await
        .unwrap();
        assert!(matches!(result, Handled::Ask { .. }));
        let current = session.as_ref().unwrap();
        assert_eq!(current.workspace.id, "tachyon");
        assert_eq!(
            current.workspace.path,
            tachyon.path().canonicalize().unwrap().to_string_lossy()
        );
        assert_eq!(
            gewu_storage::session_workspace(&state.bootstrap.pool, &current.session_id)
                .await
                .unwrap()
                .as_deref(),
            Some("tachyon")
        );
    }

    #[tokio::test]
    async fn upload_attachment_stores_outside_the_workspace_and_ask_uses_it() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let mut state = server_state_with(TestLlm::plain());
        {
            let bootstrap = &mut Arc::get_mut(&mut state).unwrap().bootstrap;
            bootstrap.working_directory = root.path().to_string_lossy().to_string();
            bootstrap.workspace_roots =
                Arc::new(BTreeMap::from([("cwd".into(), root.path().to_path_buf())]));
            bootstrap.attachment_store = store.path().to_path_buf();
        }
        let mut session = None;
        let mut running = None;

        // A session must exist before attachments can be stored (the GUI
        // creates one when the tab is used).
        let payload = b"\x89PNG data";
        let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
        let stored = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::upload_attachment(1, "shot.png", &encoded),
        )
        .await;
        assert!(
            matches!(stored, Err(RequestError::Failed(_))),
            "upload without a session must fail"
        );

        handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::ask(2, "hi"),
        )
        .await
        .unwrap();
        let session_id = session.as_ref().unwrap().session_id.clone();
        // The setup ask claimed the session; release it so later requests in
        // this test can claim again (the connection loop does this in
        // production once the turn ends).
        state.release_session(&session_id).await;

        let handled = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::upload_attachment(3, "shot.png", &encoded),
        )
        .await
        .unwrap();
        let stored_path = match handled {
            Handled::Reply(ServerMessage::AttachmentStored {
                name, path, bytes, ..
            }) => {
                assert_eq!(name, "shot.png");
                assert_eq!(bytes, payload.len() as u64);
                path
            }
            _ => panic!("expected attachment_stored"),
        };
        // Stored under the session's store directory, not the workspace.
        let stored_path = std::path::PathBuf::from(stored_path);
        assert_eq!(
            stored_path,
            store
                .path()
                .join(&session_id)
                .join("shot.png")
                .canonicalize()
                .unwrap()
        );
        assert_eq!(std::fs::read(&stored_path).unwrap(), payload);
        assert!(
            !root.path().join("shot.png").exists(),
            "the workspace must stay clean"
        );

        // The ask resolves the store name first and inlines the media.
        let handled = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::ask_with_attachments(
                4,
                "look",
                vec![gewu_protocol::AskAttachment {
                    name: "shot.png".into(),
                    mime_type: "image/png".into(),
                    data: None,
                    path: Some("shot.png".into()),
                }],
            ),
        )
        .await
        .unwrap();
        let media = match handled {
            Handled::Ask {
                input: AskInput { media, .. },
                ..
            } => media,
            _ => panic!("expected ask"),
        };
        assert_eq!(
            media,
            vec![protocol::ContentPart::Image {
                data: encoded,
                mime_type: "image/png".into(),
            }]
        );
    }

    #[tokio::test]
    async fn store_attachments_shadow_same_named_workspace_files() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        // The workspace file must NOT be the one referenced.
        std::fs::write(root.path().join("notes.txt"), "workspace copy").unwrap();
        let mut state = server_state_with(TestLlm::plain());
        {
            let bootstrap = &mut Arc::get_mut(&mut state).unwrap().bootstrap;
            bootstrap.working_directory = root.path().to_string_lossy().to_string();
            bootstrap.workspace_roots =
                Arc::new(BTreeMap::from([("cwd".into(), root.path().to_path_buf())]));
            bootstrap.attachment_store = store.path().to_path_buf();
        }
        let mut session = None;
        let mut running = None;
        handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::ask(1, "hi"),
        )
        .await
        .unwrap();
        let session_id = session.as_ref().unwrap().session_id.clone();
        state.release_session(&session_id).await;
        let store_dir = store.path().join(&session_id);
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("notes.txt"), "stored copy").unwrap();

        let handled = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::ask_with_attachments(
                2,
                "look",
                vec![gewu_protocol::AskAttachment {
                    name: "notes.txt".into(),
                    mime_type: "text/plain".into(),
                    data: None,
                    path: Some("notes.txt".into()),
                }],
            ),
        )
        .await
        .unwrap();
        let media = match handled {
            Handled::Ask {
                input: AskInput { media, .. },
                ..
            } => media,
            _ => panic!("expected ask"),
        };
        // The reference carries the absolute store path, not the workspace.
        match &media[0] {
            protocol::ContentPart::Text { text } => {
                assert!(text.contains("stored at \""), "{text}");
                assert!(
                    text.contains(&store_dir.join("notes.txt").to_string_lossy().to_string()),
                    "reference must name the stored file: {text}"
                );
                assert!(!text.contains("workspace path"), "{text}");
            }
            other => panic!("expected a text reference, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn binary_attachments_get_bash_guidance_not_read_file() {
        let store = tempfile::tempdir().unwrap();
        let mut state = server_state_with(TestLlm::plain());
        {
            let bootstrap = &mut Arc::get_mut(&mut state).unwrap().bootstrap;
            bootstrap.attachment_store = store.path().to_path_buf();
        }
        let mut session = None;
        let mut running = None;
        handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::ask(1, "hi"),
        )
        .await
        .unwrap();
        let session_id = session.as_ref().unwrap().session_id.clone();
        state.release_session(&session_id).await;

        // A real .docx is a zip container: its first bytes carry NULs, so
        // read_file would refuse to display it.
        let mut docx = vec![0x50, 0x4b, 0x03, 0x04];
        docx.extend(std::iter::repeat_n(0u8, 64));
        let encoded = base64::engine::general_purpose::STANDARD.encode(&docx);
        let handled = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::upload_attachment(2, "论文.docx", &encoded),
        )
        .await
        .unwrap();
        let stored_path = match handled {
            Handled::Reply(ServerMessage::AttachmentStored { path, .. }) => path,
            _ => panic!("expected attachment_stored"),
        };

        let handled = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::ask_with_attachments(
                3,
                "look",
                vec![gewu_protocol::AskAttachment {
                    name: "论文.docx".into(),
                    mime_type:
                        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                            .into(),
                    data: None,
                    path: Some("论文.docx".into()),
                }],
            ),
        )
        .await
        .unwrap();
        let media = match handled {
            Handled::Ask {
                input: AskInput { media, .. },
                ..
            } => media,
            _ => panic!("expected ask"),
        };
        let reference = match &media[0] {
            protocol::ContentPart::Text { text } => text.clone(),
            other => panic!("expected a text reference, got {other:?}"),
        };
        assert!(reference.contains("binary"), "{reference}");
        assert!(reference.contains("inspect it with bash"), "{reference}");
        assert!(
            reference.contains("word/document.xml"),
            "the .docx extraction hint must be present: {reference}"
        );
        assert!(
            !reference.contains("Use read_file"),
            "read_file must not be offered for binary files: {reference}"
        );
        assert!(reference.contains(&stored_path), "{reference}");
    }

    #[tokio::test]
    async fn prune_attachment_store_removes_stale_session_dirs() {
        let store = tempfile::tempdir().unwrap();
        let old_dir = store.path().join("old-session");
        let fresh_dir = store.path().join("fresh-session");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&fresh_dir).unwrap();
        std::fs::write(old_dir.join("a.txt"), "x").unwrap();
        std::fs::write(fresh_dir.join("b.txt"), "x").unwrap();
        // Backdate the file inside the old directory past the TTL (rewrites
        // do not bump a directory's mtime, so the newest file decides).
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 3600);
        let handle = std::fs::File::options()
            .write(true)
            .open(old_dir.join("a.txt"))
            .unwrap();
        handle
            .set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();
        drop(handle);

        crate::bootstrap::prune_attachment_store(store.path());

        assert!(!old_dir.exists(), "stale session dir must be pruned");
        assert!(fresh_dir.exists(), "fresh session dir must survive");
    }

    #[tokio::test]
    async fn inline_image_attachment_reaches_the_agent_memory() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let mut state = server_state_with(TestLlm::plain());
        {
            let bootstrap = &mut Arc::get_mut(&mut state).unwrap().bootstrap;
            bootstrap.pool = pool;
        }
        let mut session = None;
        let mut running = None;
        let handled = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::ask_with_attachments(
                1,
                "describe this",
                vec![gewu_protocol::AskAttachment {
                    name: "shot.png".into(),
                    mime_type: "image/png".into(),
                    data: Some("aGk=".into()),
                    path: None,
                }],
            ),
        )
        .await
        .unwrap();
        let (request_id, media) = match handled {
            Handled::Ask {
                request_id,
                input: AskInput { media, .. },
                ..
            } => (request_id, media),
            _ => panic!("expected ask"),
        };
        assert_eq!(
            media,
            vec![protocol::ContentPart::Image {
                data: "aGk=".into(),
                mime_type: "image/png".into(),
            }]
        );

        // Running the turn stores the prompt and the attachment in memory.
        let session = session.clone().unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        execute_ask(
            &session,
            &request_id,
            "describe this",
            media,
            &pool,
            |_| async { Ok(()) },
        )
        .await;
        let messages = session.agent.lock().await.memory.messages().to_vec();
        let user = messages
            .iter()
            .find_map(|message| match message {
                gewu_core::Message::User { content } => Some(content),
                _ => None,
            })
            .expect("user message stored");
        assert_eq!(user.len(), 2);
        assert!(
            matches!(&user[0], protocol::ContentPart::Text { text } if text == "describe this")
        );
        assert!(matches!(&user[1], protocol::ContentPart::Image { .. }));
    }

    #[tokio::test]
    async fn path_attachments_inline_media_and_reference_other_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("shot.png"), [0x89, 0x50, 0x4e, 0x47]).unwrap();
        std::fs::write(root.path().join("notes.txt"), "hello").unwrap();
        let mut state = server_state_with(TestLlm::plain());
        {
            let bootstrap = &mut Arc::get_mut(&mut state).unwrap().bootstrap;
            bootstrap.working_directory = root.path().to_string_lossy().to_string();
            bootstrap.workspace_roots =
                Arc::new(BTreeMap::from([("cwd".into(), root.path().to_path_buf())]));
        }
        let mut session = None;
        let mut running = None;
        let handled = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::ask_with_attachments(
                1,
                "look",
                vec![
                    gewu_protocol::AskAttachment {
                        name: "shot.png".into(),
                        mime_type: "image/png".into(),
                        data: None,
                        path: Some("shot.png".into()),
                    },
                    gewu_protocol::AskAttachment {
                        name: "notes.txt".into(),
                        mime_type: "text/plain".into(),
                        data: None,
                        path: Some("notes.txt".into()),
                    },
                ],
            ),
        )
        .await
        .unwrap();
        let media = match handled {
            Handled::Ask {
                input: AskInput { media, .. },
                ..
            } => media,
            _ => panic!("expected ask"),
        };
        assert_eq!(
            media,
            vec![
                protocol::ContentPart::Image {
                    data: base64::engine::general_purpose::STANDARD
                        .encode([0x89, 0x50, 0x4e, 0x47]),
                    mime_type: "image/png".into(),
                },
                protocol::ContentPart::Text { text: "[Attached file \"notes.txt\" (5 B) at workspace path \"notes.txt\". Use read_file to inspect it.]".into() },
            ]
        );
    }

    #[tokio::test]
    async fn path_attachments_cannot_escape_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.png");
        std::fs::write(&secret, [0x89, 0x50, 0x4e, 0x47]).unwrap();
        let mut state = server_state_with(TestLlm::plain());
        {
            let bootstrap = &mut Arc::get_mut(&mut state).unwrap().bootstrap;
            bootstrap.working_directory = root.path().to_string_lossy().to_string();
            bootstrap.workspace_roots =
                Arc::new(BTreeMap::from([("cwd".into(), root.path().to_path_buf())]));
            bootstrap.enforcement = PathEnforcement::Contain;
        }
        let relative = secret
            .strip_prefix(root.path())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| format!("../{}", secret.file_name().unwrap().to_string_lossy()));
        let mut session = None;
        let mut running = None;
        let result = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::ask_with_attachments(
                1,
                "look",
                vec![gewu_protocol::AskAttachment {
                    name: "secret.png".into(),
                    mime_type: "image/png".into(),
                    data: None,
                    path: Some(relative),
                }],
            ),
        )
        .await;
        assert!(
            matches!(
                result,
                Err(RequestError::Failed(_)) | Err(RequestError::Invalid(_))
            ),
            "an outside-workspace attachment must be rejected"
        );
    }

    #[tokio::test]
    async fn inline_attachments_reject_unsupported_mime_and_oversize() {
        let state = server_state_with(TestLlm::plain());
        let mut session = None;
        let mut running = None;

        let bad_mime = handle_request(
            &state.clone(),
            &mut session,
            &mut running,
            ClientRequest::ask_with_attachments(
                1,
                "look",
                vec![gewu_protocol::AskAttachment {
                    name: "notes.txt".into(),
                    mime_type: "text/plain".into(),
                    data: Some("aGk=".into()),
                    path: None,
                }],
            ),
        )
        .await;
        assert!(
            matches!(bad_mime, Err(RequestError::Invalid(_))),
            "unsupported inline MIME must be rejected"
        );

        let oversized = vec![0u8; MAX_ATTACHMENT_BYTES + 1];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&oversized);
        let too_big = handle_request(
            &state,
            &mut session,
            &mut running,
            ClientRequest::ask_with_attachments(
                2,
                "look",
                vec![gewu_protocol::AskAttachment {
                    name: "big.png".into(),
                    mime_type: "image/png".into(),
                    data: Some(encoded),
                    path: None,
                }],
            ),
        )
        .await;
        assert!(
            matches!(too_big, Err(RequestError::Invalid(_))),
            "oversized inline attachment must be rejected"
        );
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
            plan["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "write_file"),
            "plan advertises editing tools and refuses them at execution"
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
        assert_eq!(
            build["tools"], plan["tools"],
            "mode switches must keep the tool prefix identical"
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
                plugins: Default::default(),
                skills: Vec::new(),
                log_writer: None,
                model: "test-model".to_string(),
                provider_name: "test".to_string(),
                working_directory: "/tmp/gewu-test".to_string(),
                workspace_roots: Arc::new(BTreeMap::from([(
                    "cwd".to_string(),
                    std::path::PathBuf::from("/tmp/gewu-test"),
                )])),
                enforcement: PathEnforcement::None,
                image_limits: gewu_tools::ImageLimits::default(),
                context_limit: None,
                models: BTreeMap::from([(
                    "test-model".to_string(),
                    crate::bootstrap::ModelEntry {
                        provider: TestLlm::plain(),
                        provider_name: "test".to_string(),
                        label: "test-model".to_string(),
                        context_limit: 256_000,
                        default_effort: None,
                        effort_providers: BTreeMap::new(),
                        supports_reasoning_effort: false,
                    },
                )]),
                model_selector: "test-model".to_string(),
                agents: Arc::new(BTreeMap::new()),
                attachment_store: std::env::temp_dir().join("gewu-test-attachments"),
            },
            token: None,
            busy_sessions: Mutex::new(HashSet::new()),
            sessions: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            workflow_progress: tokio::sync::broadcast::channel(16).0,
            workflows: Mutex::new(HashMap::new()),
            questions: Arc::new(gewu_core::QuestionBroker::new()),
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
        // Hold the first provider response until the steer has been queued,
        // so the test always exercises the mid-flight path instead of racing
        // the first request build.
        let llm = RecordingLlm::gated();
        let state = server_state_with(llm.clone());
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        in_tx
            .send(r#"{"id": 1, "op": "ask", "prompt": "go"}"#.to_string())
            .await
            .unwrap();
        let ready = recv_json(&mut out_rx).await;
        assert_eq!(ready["type"], "ready");
        let session_id = ready["session_id"].as_str().unwrap().to_string();

        llm.wait_for_first_send().await;
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
        llm.release_first_send();

        // The steered message extends the run; wait for the final result.
        loop {
            if recv_json(&mut out_rx).await["type"] == "result" {
                break;
            }
        }

        let requests = llm.requests();
        assert_eq!(
            requests.len(),
            2,
            "steer must trigger a follow-up request: {requests:?}"
        );
        assert!(
            requests[0].iter().any(|prompt| prompt.contains("go")),
            "first request should contain the original prompt: {requests:?}"
        );
        assert!(
            !requests[0]
                .iter()
                .any(|prompt| prompt.contains("focus on tests")),
            "steer must not be in the first request: {requests:?}"
        );
        assert!(
            requests[1]
                .iter()
                .any(|prompt| prompt.contains("focus on tests")),
            "steered text never reached the follow-up request: {requests:?}"
        );

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
            let inbox = session.inbox.steering.clone();
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
    async fn steer_queued_during_interrupt_cleanup_resumes() {
        let llm = RecordingLlm::gated();
        let state = server_state_with(llm.clone());
        let (input, mut output, connection) = spawn_connection(state.clone()).await;
        input
            .send(r#"{"id":1,"op":"ask","prompt":"cancel this"}"#.into())
            .await
            .unwrap();
        let ready = recv_json(&mut output).await;
        let id = ready["session_id"].as_str().unwrap();
        let session = state.sessions.lock().await[id].clone();
        llm.wait_for_first_send().await;

        let agent_lock = session.agent.lock();
        tokio::pin!(agent_lock);
        assert!(futures::poll!(&mut agent_lock).is_pending());
        session.inbox.steering.send("discard this steer").unwrap();
        input
            .send(r#"{"id":2,"op":"interrupt"}"#.into())
            .await
            .unwrap();
        let agent = tokio::time::timeout(std::time::Duration::from_secs(5), agent_lock)
            .await
            .expect("interrupt did not release the worker's agent lock");
        assert!(!session.inbox.steering.is_pending());
        session.inbox.steering.send("keep this steer").unwrap();
        drop(agent);

        loop {
            let message = recv_json(&mut output).await;
            if message["type"] == "result" {
                assert_ne!(message["request_id"], ready["request_id"]);
                break;
            }
        }
        let requests = llm.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]
                .iter()
                .any(|text| text.contains("keep this steer"))
        );
        assert!(
            !requests[1]
                .iter()
                .any(|text| text.contains("discard this steer"))
        );

        drop(input);
        connection.await.unwrap().unwrap();
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
            reasoning_effort: None,
            context_limit: 256_000,
            tools: vec!["echo".to_string()],
            inbox: Arc::new(SessionInbox::default()),
            workspace: WorkspaceSummary {
                id: "cwd".into(),
                path: "/tmp/gewu-test".into(),
            },
        };

        let (tx, mut rx) = mpsc::channel::<String>(256);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let handle = tokio::spawn({
            let session = session.clone();
            async move {
                execute_ask(&session, "req-test", "go", Vec::new(), &pool, |msg| {
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
        // never produced output (Bug 11). Hold the first response so the
        // interrupt always lands after the first request was built.
        let llm = RecordingLlm::gated();
        let state = server_state_with(llm.clone() as Arc<dyn gewu_core::traits::LlmProvider>);
        let (in_tx, mut out_rx, handle) = spawn_connection(state).await;

        in_tx
            .send(r#"{"id": 1, "op": "ask", "prompt": "write an essay"}"#.to_string())
            .await
            .unwrap();
        let ready = recv_json(&mut out_rx).await;
        assert_eq!(ready["type"], "ready");

        // Interrupt after the first request is in flight but before it
        // produced any output.
        llm.wait_for_first_send().await;
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

        let requests = llm.requests();
        assert_eq!(
            requests.len(),
            2,
            "interrupt and follow-up should each make one request: {requests:?}"
        );
        assert!(
            requests[0]
                .iter()
                .any(|prompt| prompt.contains("write an essay")),
            "the interrupted request should have reached the model: {requests:?}"
        );
        assert!(
            requests[1]
                .iter()
                .all(|prompt| !prompt.contains("write an essay")),
            "aborted prompt leaked into the next LLM request: {requests:?}"
        );
        assert!(
            requests[1].iter().any(|prompt| prompt.contains("Say ok")),
            "the new prompt should reach the LLM: {requests:?}"
        );

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    async fn next_question_request(out_rx: &mut mpsc::Receiver<String>) -> String {
        loop {
            let frame = recv_json(out_rx).await;
            assert_ne!(frame["type"], "error", "{frame}");
            if frame["type"] == "event" && frame["event"]["type"] == "question_asked" {
                assert!(frame["event"]["questions"][0]["question"].is_string());
                return frame["event"]["request_id"]
                    .as_str()
                    .expect("question request id")
                    .to_string();
            }
            assert_ne!(frame["type"], "result", "turn ended before asking");
        }
    }

    #[tokio::test]
    async fn question_tool_waits_for_answer_and_continues() {
        let state = server_state_with(TestLlm::question_call());
        let (input, mut output, connection) = spawn_connection(state.clone()).await;
        input
            .send(
                ClientRequest::ask(1, "pick a database")
                    .with_session("question-answered")
                    .to_line(),
            )
            .await
            .unwrap();
        assert_eq!(recv_json(&mut output).await["type"], "ready");

        let request_id = next_question_request(&mut output).await;
        assert_eq!(state.questions.pending_count(), 1);

        input
            .send(
                ClientRequest::question_reply(2, &request_id, vec![vec!["SQLite".to_string()]])
                    .to_line(),
            )
            .await
            .unwrap();

        let mut replied = false;
        let mut resolved = None;
        loop {
            let frame = recv_json(&mut output).await;
            match frame["type"].as_str().expect("message type") {
                "question_replied" => {
                    assert_eq!(frame["request_id"], request_id);
                    assert_eq!(frame["answers"][0][0], "SQLite");
                    replied = true;
                }
                "event" if frame["event"]["type"] == "question_resolved" => {
                    assert_eq!(frame["event"]["request_id"], request_id);
                    resolved = Some(frame["event"]["answers"].clone());
                }
                "result" => break,
                "error" => panic!("unexpected error: {frame}"),
                _ => {}
            }
        }
        assert!(replied, "the reply was not acknowledged");
        assert_eq!(
            resolved,
            Some(serde_json::json!([["SQLite"]])),
            "the resolved event did not carry the answer"
        );
        assert_eq!(state.questions.pending_count(), 0);

        // The model-facing tool result names the selected label.
        let session = state
            .sessions
            .lock()
            .await
            .get("question-answered")
            .cloned()
            .expect("session");
        let messages = session.agent.lock().await.memory.messages().to_vec();
        assert!(messages.iter().any(|message| matches!(
            message,
            Message::Tool { content, .. } if content.contains("\"Which database?\"=\"SQLite\"")
        )));

        drop(input);
        connection.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn interrupting_a_pending_question_unregisters_it() {
        let state = server_state_with(TestLlm::question_call());
        let (input, mut output, connection) = spawn_connection(state.clone()).await;
        input
            .send(
                ClientRequest::ask(1, "pick a database")
                    .with_session("question-interrupted")
                    .to_line(),
            )
            .await
            .unwrap();
        assert_eq!(recv_json(&mut output).await["type"], "ready");

        let _request_id = next_question_request(&mut output).await;
        assert_eq!(state.questions.pending_count(), 1);

        input
            .send(ClientRequest::interrupt(2).to_line())
            .await
            .unwrap();
        loop {
            let frame = recv_json(&mut output).await;
            if frame["type"] == "interrupted"
                || (frame["type"] == "error" && frame["error"] == "interrupted")
            {
                break;
            }
            assert_ne!(frame["type"], "error", "{frame}");
        }
        assert_eq!(
            state.questions.pending_count(),
            0,
            "the interrupted tool left its question registered"
        );

        drop(input);
        connection.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn dismissed_question_returns_a_normal_tool_result() {
        let state = server_state_with(TestLlm::question_call());
        let (input, mut output, connection) = spawn_connection(state.clone()).await;
        input
            .send(
                ClientRequest::ask(1, "pick a database")
                    .with_session("question-dismissed")
                    .to_line(),
            )
            .await
            .unwrap();
        assert_eq!(recv_json(&mut output).await["type"], "ready");

        let request_id = next_question_request(&mut output).await;
        input
            .send(ClientRequest::question_reject(2, &request_id).to_line())
            .await
            .unwrap();

        let mut resolved = false;
        loop {
            let frame = recv_json(&mut output).await;
            match frame["type"].as_str().expect("message type") {
                "question_replied" => {
                    assert_eq!(frame["request_id"], request_id);
                    assert!(frame.get("answers").is_none(), "{frame}");
                }
                "event"
                    if frame["event"]["type"] == "question_resolved"
                        && frame["event"]["answers"].is_null() =>
                {
                    resolved = true;
                }
                "result" => break,
                "error" => panic!("unexpected error: {frame}"),
                _ => {}
            }
        }
        assert!(resolved, "no dismissal was reported");
        assert_eq!(state.questions.pending_count(), 0);

        let session = state
            .sessions
            .lock()
            .await
            .get("question-dismissed")
            .cloned()
            .expect("session");
        let messages = session.agent.lock().await.memory.messages().to_vec();
        assert!(messages.iter().any(|message| matches!(
            message,
            Message::Tool { content, .. } if content.contains("dismissed this question")
        )));

        drop(input);
        connection.await.unwrap().unwrap();
    }
}
