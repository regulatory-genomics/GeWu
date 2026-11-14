//! Async Rust client for the GeWu server (`gewu-server serve`).
//!
//! [`Client`] connects over TCP (newline-delimited JSON, default port 8421)
//! or WebSocket (default port 8420), sends prompts, and yields the agent's
//! streamed events as they happen.
//!
//! # Example
//!
//! ```no_run
//! use gewu_client::Client;
//!
//! # async fn demo() -> gewu_client::Result<()> {
//! let client = Client::tcp("127.0.0.1", 8421).await?;
//!
//! let mut ask = client.ask("List the Rust files in src/").send().await?;
//! while let Some(frame) = ask.next_frame().await {
//!     match frame? {
//!         gewu_protocol::ServerMessage::Event { event, .. } => {
//!             if let Ok(gewu_protocol::AgentEvent::ContentDelta { delta }) =
//!                 serde_json::from_value::<gewu_protocol::AgentEvent>(event)
//!             {
//!                 print!("{delta}");
//!             }
//!         }
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! One `ask` runs at a time per connection; send
//! [`Client::interrupt`](Client::interrupt) to cancel the running one.
//! Prompts on the same connection share a conversation; pass an explicit
//! `session_id` (from a previous run, via [`AskBuilder::session_id`]) to
//! resume one.

mod connection;
mod inner;
pub mod session;
mod stream;

pub use session::{Block, Phase, SessionModel};
pub use stream::AskStream;

use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use gewu_protocol::{
    ClientRequest, DirectoryEntry, FileEntry, Message, ServerMessage, SessionSummary, TokenUsage,
    WorkspaceSummary,
};
use inner::Command;

/// Alias for a [`Result`](std::result::Result) with this crate's error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Server model/provider plus the configured filesystem roots and the active
/// root for the requested session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    /// Model name, e.g. `gpt-4o`.
    pub model: String,
    /// Provider name, e.g. `openai`.
    pub provider: String,
    /// The working directory the server's agent operates in (absolute).
    pub working_directory: String,
    pub workspaces: Vec<WorkspaceSummary>,
    pub active_workspace: String,
}

/// The session's context usage against the agent's context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// Estimated tokens in the conversation (the same estimate compaction
    /// uses); `0` when the server has no session to measure.
    pub used_tokens: u64,
    /// The agent's context window in tokens; `0` when unknown.
    pub context_limit: u64,
    /// Whether an ask is currently running on the connection. When `true`,
    /// the numeric fields are `0` because the running ask holds the agent
    /// lock; query again once it finishes. `0/0` with `busy: false` means
    /// the connection has no session yet.
    pub busy: bool,
    /// Provider-reported token accounting accumulated across the session
    /// (uncached input, cache hits, cache writes, output). `None` until the
    /// provider reports usage; from it, `TokenUsage::cache_hit_rate_percent`
    /// gives the session cache-hit rate.
    pub provider_usage: Option<TokenUsage>,
}

/// Errors produced by the client.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// The transport failed (connect error, IO error, WebSocket error).
    #[error("connection error: {0}")]
    Connection(String),
    /// The server reported an error for this request.
    #[error("server error: {0}")]
    Server(String),
    /// The server sent an unexpected frame.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// An ask is already running on this connection; interrupt it first.
    #[error("an ask is already in progress; interrupt it first")]
    Busy,
    /// The server rejected the authentication token.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// The ask was aborted by [`Client::interrupt`](Client::interrupt); the
    /// server does not send a terminal frame for it, so the ask's stream
    /// ends with this error instead.
    #[error("the ask was interrupted")]
    Interrupted,
}

/// Handle to a running connection actor. Cloning is cheap; all clones share
/// one connection, and the connection closes when all handles are dropped or
/// [`Client::close`] completes.
#[derive(Clone)]
pub struct Client {
    commands: mpsc::Sender<Command>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
}

/// Builder for an [`ask`](Client::ask) with optional session/agent fields.
#[must_use = "call `.send().await` on the builder to start the ask"]
pub struct AskBuilder<'a> {
    client: &'a Client,
    prompt: String,
    session_id: Option<String>,
    agent: Option<String>,
    workspace: Option<String>,
}

impl Client {
    /// Observe this session across turn boundaries until the connection closes.
    pub async fn watch_session(&self, session_id: impl Into<String>) -> Result<AskStream> {
        let request = ClientRequest::subscribe_session(self.next_correlation_id(), session_id);
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Subscribe {
                request,
                stream: tx,
            })
            .await
            .map_err(|_| Error::Connection("connection closed".into()))?;
        Ok(AskStream::new(rx.await.map_err(|_| {
            Error::Connection("connection closed".into())
        })??))
    }
    /// Restore a session's current transcript and attach to its active turn.
    pub async fn subscribe_session(&self, session_id: impl Into<String>) -> Result<AskStream> {
        let request = ClientRequest::subscribe_session(self.next_correlation_id(), session_id);
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Ask {
                request,
                stream: tx,
            })
            .await
            .map_err(|_| Error::Connection("connection closed".into()))?;
        Ok(AskStream::new(rx.await.map_err(|_| {
            Error::Connection("connection closed".into())
        })??))
    }
    pub async fn revert_chat(
        &self,
        turn: Option<usize>,
        undo: bool,
    ) -> Result<(String, Vec<Message>)> {
        match self
            .request(ClientRequest::revert_chat(
                self.next_correlation_id(),
                turn,
                undo,
            ))
            .await?
        {
            ServerMessage::Session {
                session_id,
                messages,
                ..
            } => Ok((session_id, messages)),
            ServerMessage::Error { error, .. } => Err(Error::Server(error)),
            other => Err(unexpected(other, "session")),
        }
    }
    /// Switch profiles on an idle session, retaining its stored history.
    pub async fn switch_agent(&self, agent: impl Into<String>) -> Result<(String, Vec<String>)> {
        let request = ClientRequest::switch_agent(self.next_correlation_id(), agent);
        match self.request(request).await? {
            ServerMessage::AgentSwitched {
                session_id, tools, ..
            } => Ok((session_id, tools)),
            ServerMessage::Error { error, .. } => Err(Error::Server(error)),
            other => Err(unexpected(other, "agent_switched")),
        }
    }
    /// Every switchable model as `endpoint/model` selectors with display labels.
    pub async fn list_models(&self) -> Result<Vec<gewu_protocol::ModelSummary>> {
        let request = ClientRequest::list_models(self.next_correlation_id());
        match self.request(request).await? {
            ServerMessage::Models { models, .. } => Ok(models),
            ServerMessage::Error { error, .. } => Err(Error::Server(error)),
            other => Err(unexpected(other, "models")),
        }
    }
    /// Switch the LLM model on an idle session, retaining its stored history.
    /// Returns the session id and the new model's display label.
    pub async fn switch_model(&self, model: impl Into<String>) -> Result<(String, String)> {
        let request = ClientRequest::switch_model(self.next_correlation_id(), model);
        match self.request(request).await? {
            ServerMessage::ModelSwitched {
                session_id, label, ..
            } => Ok((session_id, label)),
            ServerMessage::Error { error, .. } => Err(Error::Server(error)),
            other => Err(unexpected(other, "model_switched")),
        }
    }

    /// Switch the active filesystem root on an idle session.
    pub async fn switch_workspace(
        &self,
        workspace: impl Into<String>,
    ) -> Result<(String, WorkspaceSummary)> {
        let request = ClientRequest::switch_workspace(self.next_correlation_id(), workspace);
        match self.request(request).await? {
            ServerMessage::WorkspaceSwitched {
                session_id,
                workspace,
                ..
            } => Ok((session_id, workspace)),
            ServerMessage::Error { error, .. } => Err(Error::Server(error)),
            other => Err(unexpected(other, "workspace_switched")),
        }
    }

    /// Browse direct directories on the GeWu server host.
    pub async fn browse_workspace(
        &self,
        path: impl Into<String>,
    ) -> Result<(String, Option<String>, Vec<DirectoryEntry>)> {
        let request = ClientRequest::browse_workspace(self.next_correlation_id(), path);
        match self.request(request).await? {
            ServerMessage::Directories {
                path,
                parent,
                entries,
                ..
            } => Ok((path, parent, entries)),
            ServerMessage::Error { error, .. } => Err(Error::Server(error)),
            other => Err(unexpected(other, "directories")),
        }
    }

    /// Add and select a server-host directory for this session.
    pub async fn add_workspace(
        &self,
        path: impl Into<String>,
    ) -> Result<(String, WorkspaceSummary)> {
        let request = ClientRequest::add_workspace(self.next_correlation_id(), path);
        match self.request(request).await? {
            ServerMessage::WorkspaceAdded {
                session_id,
                workspace,
                ..
            } => Ok((session_id, workspace)),
            ServerMessage::Error { error, .. } => Err(Error::Server(error)),
            other => Err(unexpected(other, "workspace_added")),
        }
    }
    /// Connect over TCP (newline-delimited JSON; the server listens on
    /// port 8421 by default).
    pub async fn tcp(host: &str, port: u16) -> Result<Self> {
        Self::tcp_with_token(host, port, None).await
    }

    /// Connect over TCP, authenticating with `token` when the server runs
    /// with `--token`.
    pub async fn tcp_with_token(host: &str, port: u16, token: Option<&str>) -> Result<Self> {
        let conn = connection::Connection::tcp(&format!("{host}:{port}")).await?;
        let conn = authenticate(conn, token).await?;
        Ok(Self::spawn(conn))
    }

    /// Connect over WebSocket (`ws://` or `wss://`; the server listens on
    /// port 8420 by default).
    pub async fn websocket(url: &str) -> Result<Self> {
        Self::websocket_with_token(url, None).await
    }

    /// Connect over WebSocket, authenticating with `token` when the server
    /// runs with `--token`.
    pub async fn websocket_with_token(url: &str, token: Option<&str>) -> Result<Self> {
        let conn = connection::Connection::websocket(url).await?;
        let conn = authenticate(conn, token).await?;
        Ok(Self::spawn(conn))
    }

    fn spawn(conn: connection::Connection) -> Self {
        let (tx, rx) = mpsc::channel::<Command>(64);
        tokio::spawn(inner::run(inner::Inner::new(conn, rx)));
        Self {
            commands: tx,
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    fn next_correlation_id(&self) -> serde_json::Value {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        serde_json::Value::Number(id.into())
    }

    /// Begin an ask. The returned builder is completed with
    /// [`AskBuilder::send`], which returns an [`AskStream`] yielding the
    /// agent's frames until the ask completes (terminal `result`) or fails
    /// (terminal `error`).
    ///
    /// Only one ask may run per connection at a time; starting another
    /// returns [`Error::Busy`] until it completes or
    /// [`interrupt`](Client::interrupt) is called.
    pub fn ask(&self, prompt: impl Into<String>) -> AskBuilder<'_> {
        AskBuilder {
            client: self,
            prompt: prompt.into(),
            session_id: None,
            agent: None,
            workspace: None,
        }
    }

    /// Interrupt the running ask, if any. Resolves once the server has
    /// aborted it; the aborted ask's own stream ends with
    /// [`Error::Interrupted`] (the `interrupted` frame is delivered on this
    /// interrupt's correlation id), and the connection accepts a new ask.
    pub async fn interrupt(&self) -> Result<()> {
        let request = ClientRequest::interrupt(self.next_correlation_id());
        self.request(request).await.map(|_| ())
    }

    /// Inject a steering message into this connection's running ask.
    ///
    /// The message joins the conversation at the agent's next round
    /// boundary and also arrives as a `steered` agent event on the ask's
    /// stream. Fails when no ask is running.
    pub async fn steer(&self, text: impl Into<String>) -> Result<()> {
        let request = ClientRequest::steer(self.next_correlation_id(), text);
        match self.request(request).await? {
            ServerMessage::Steered { .. } => Ok(()),
            other => Err(unexpected(other, "steered")),
        }
    }

    /// List the tool names available to this connection's agent.
    pub async fn list_tools(&self) -> Result<Vec<String>> {
        let request = ClientRequest::list_tools(self.next_correlation_id());
        match self.request(request).await? {
            ServerMessage::Tools { tools, .. } => Ok(tools),
            other => Err(unexpected(other, "tools")),
        }
    }

    pub async fn list_files(&self, path: impl Into<String>) -> Result<Vec<FileEntry>> {
        let path = path.into();
        let request = ClientRequest::list_files(self.next_correlation_id(), path.clone());
        match self.request(request).await? {
            ServerMessage::Files { entries, .. } => Ok(entries),
            other => Err(unexpected(other, "files")),
        }
    }

    pub async fn read_file(&self, path: impl Into<String>) -> Result<(String, bool)> {
        let path = path.into();
        let request = ClientRequest::read_file(self.next_correlation_id(), path);
        match self.request(request).await? {
            ServerMessage::FileContent {
                content, truncated, ..
            } => Ok((content, truncated)),
            other => Err(unexpected(other, "file_content")),
        }
    }

    /// Write (create or overwrite) a file in the server's working-directory
    /// root. `path` must be a bare file name; returns the name used and the
    /// number of bytes written.
    pub async fn write_file(&self, path: impl Into<String>, bytes: &[u8]) -> Result<(String, u64)> {
        use base64::Engine as _;
        let content_base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        let request = ClientRequest::write_file(self.next_correlation_id(), path, content_base64);
        match self.request(request).await? {
            ServerMessage::FileWritten { path, bytes, .. } => Ok((path, bytes)),
            other => Err(unexpected(other, "file_written")),
        }
    }

    /// List the available agent profiles (build/explore/general).
    pub async fn list_agents(&self) -> Result<Vec<String>> {
        let request = ClientRequest::list_agents(self.next_correlation_id());
        match self.request(request).await? {
            ServerMessage::Agents { agents, .. } => Ok(agents),
            other => Err(unexpected(other, "agents")),
        }
    }

    /// List persisted conversation sessions, newest first.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let request = ClientRequest::list_sessions(self.next_correlation_id());
        match self.request(request).await? {
            ServerMessage::Sessions { sessions, .. } => Ok(sessions),
            other => Err(unexpected(other, "sessions")),
        }
    }

    /// Load one persisted session's messages, oldest first.
    pub async fn get_session(&self, session_id: impl Into<String>) -> Result<Vec<Message>> {
        let request = ClientRequest::get_session(self.next_correlation_id(), session_id);
        match self.request(request).await? {
            ServerMessage::Session { messages, .. } => Ok(messages),
            other => Err(unexpected(other, "session")),
        }
    }

    /// Delete a persisted session's messages from the store.
    pub async fn delete_session(&self, session_id: impl Into<String>) -> Result<()> {
        let request = ClientRequest::delete_session(self.next_correlation_id(), session_id);
        match self.request(request).await? {
            ServerMessage::Deleted { .. } => Ok(()),
            other => Err(unexpected(other, "deleted")),
        }
    }

    /// Liveness check.
    pub async fn ping(&self) -> Result<()> {
        let request = ClientRequest::ping(self.next_correlation_id());
        self.request(request).await.map(|_| ())
    }

    /// The model, provider, and working directory the server runs with.
    pub async fn info(&self) -> Result<ServerInfo> {
        let request = ClientRequest::info(self.next_correlation_id());
        self.info_request(request).await
    }

    /// Query server/session metadata for a particular persisted session.
    pub async fn info_for_session(&self, session_id: &str) -> Result<ServerInfo> {
        let request = ClientRequest::info(self.next_correlation_id()).with_session(session_id);
        self.info_request(request).await
    }

    async fn info_request(&self, request: ClientRequest) -> Result<ServerInfo> {
        match self.request(request).await? {
            ServerMessage::Info {
                model,
                provider,
                working_directory,
                workspaces,
                active_workspace,
                ..
            } => Ok(ServerInfo {
                model,
                provider,
                working_directory,
                workspaces,
                active_workspace,
            }),
            other => Err(unexpected(other, "info")),
        }
    }

    /// The session's context usage against the agent's context window.
    ///
    /// Pass the session id to have the server create/load that session before
    /// measuring (so a resumed conversation reports real numbers). While an
    /// ask is streaming the reply carries zeros with `busy: true` — query
    /// when idle, or check [`Usage::busy`].
    pub async fn usage(&self, session_id: Option<&str>) -> Result<Usage> {
        let mut request = ClientRequest::usage(self.next_correlation_id());
        if let Some(session_id) = session_id {
            request = request.with_session(session_id);
        }
        match self.request(request).await? {
            ServerMessage::Usage {
                used_tokens,
                context_limit,
                busy,
                provider_usage,
                ..
            } => Ok(Usage {
                used_tokens,
                context_limit,
                busy,
                provider_usage,
            }),
            other => Err(unexpected(other, "usage")),
        }
    }

    /// The session id handling this connection's conversation, once known
    /// (i.e. after an ask has started). Persist it to resume the same
    /// conversation later via [`AskBuilder::session_id`].
    pub async fn session_id(&self) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::GetSession { reply: tx })
            .await
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Close the connection, detaching from any server-owned turn. Other `Client`
    /// clones become unusable too; in-flight calls fail with
    /// [`Error::Connection`].
    pub async fn close(&self) {
        let (tx, rx) = oneshot::channel();
        if self
            .commands
            .send(Command::Close { ack: tx })
            .await
            .is_err()
        {
            return; // already closed
        }
        let _ = rx.await;
    }

    /// Send a request and await its single reply frame.
    async fn request(&self, request: ClientRequest) -> Result<ServerMessage> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Request { request, reply: tx })
            .await
            .map_err(|_| Error::Connection("client is closed".to_string()))?;
        rx.await
            .map_err(|_| Error::Connection("connection actor stopped".to_string()))?
    }
}

impl AskBuilder<'_> {
    /// Resume a specific session (honored when the connection's session is
    /// first created; otherwise the existing session keeps being used).
    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Use a specific agent profile (honored when the connection's session
    /// is first created; defaults to the server's `build`).
    pub fn agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = Some(agent.into());
        self
    }

    /// Use a workspace when this ask creates the connection's session.
    pub fn workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    /// Start the ask and return its event stream.
    pub async fn send(self) -> Result<AskStream> {
        let request = ClientRequest::ask(self.client.next_correlation_id(), self.prompt);
        let request = ClientRequest {
            session_id: self.session_id,
            agent: self.agent,
            workspace: self.workspace,
            ..request
        };
        let (tx, rx) = oneshot::channel();
        self.client
            .commands
            .send(Command::Ask {
                request,
                stream: tx,
            })
            .await
            .map_err(|_| Error::Connection("client is closed".to_string()))?;
        let handle = rx
            .await
            .map_err(|_| Error::Connection("connection actor stopped".to_string()))??;
        Ok(AskStream::new(handle))
    }
}

fn unexpected(msg: ServerMessage, expected: &str) -> Error {
    Error::Protocol(format!(
        "expected '{}' reply, got '{}'",
        expected,
        msg.kind()
    ))
}

/// Perform the `auth` handshake on a fresh connection, when a token is set.
///
/// No-op when `token` is `None` (servers without `--token` accept any first
/// request). Consumes and returns the connection so it can be handed to the
/// connection actor only after authentication succeeded.
async fn authenticate(
    mut conn: connection::Connection,
    token: Option<&str>,
) -> Result<connection::Connection> {
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return Ok(conn);
    };
    let request = ClientRequest::auth(0u64, token);
    conn.send(&request.to_line()).await?;
    loop {
        match conn.next_message().await {
            Some(Ok(line)) => match serde_json::from_str::<ServerMessage>(&line) {
                Ok(ServerMessage::Auth { ok: true, .. }) => return Ok(conn),
                Ok(ServerMessage::Auth { .. }) => {
                    return Err(Error::Auth("the server rejected the token".to_string()));
                }
                // Unrelated frames before the auth reply are ignored.
                Ok(_) => continue,
                Err(_) => continue,
            },
            Some(Err(e)) => return Err(e),
            None => {
                return Err(Error::Connection(
                    "connection closed during authentication".to_string(),
                ));
            }
        }
    }
}
