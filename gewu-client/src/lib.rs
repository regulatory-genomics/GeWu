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

use gewu_protocol::{ClientRequest, Message, ServerMessage, SessionSummary};
use inner::Command;

/// Alias for a [`Result`](std::result::Result) with this crate's error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Model and provider the server was started with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    /// Model name, e.g. `gpt-4o`.
    pub model: String,
    /// Provider name, e.g. `openai`.
    pub provider: String,
}

/// The session's context usage against the agent's context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// Estimated tokens in the conversation (the same estimate compaction
    /// uses); `0` when the server has no session to measure.
    pub used_tokens: u64,
    /// The agent's context window in tokens; `0` when unknown.
    pub context_limit: u64,
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
}

impl Client {
    /// Connect over TCP (newline-delimited JSON; the server listens on
    /// port 8421 by default).
    pub async fn tcp(host: &str, port: u16) -> Result<Self> {
        let conn = connection::Connection::tcp(&format!("{host}:{port}")).await?;
        Ok(Self::spawn(conn))
    }

    /// Connect over WebSocket (`ws://` or `wss://`; the server listens on
    /// port 8420 by default).
    pub async fn websocket(url: &str) -> Result<Self> {
        let conn = connection::Connection::websocket(url).await?;
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

    /// List the tool names available to this connection's agent.
    pub async fn list_tools(&self) -> Result<Vec<String>> {
        let request = ClientRequest::list_tools(self.next_correlation_id());
        match self.request(request).await? {
            ServerMessage::Tools { tools, .. } => Ok(tools),
            other => Err(unexpected(other, "tools")),
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

    /// The model and provider the server was started with.
    pub async fn info(&self) -> Result<ServerInfo> {
        let request = ClientRequest::info(self.next_correlation_id());
        match self.request(request).await? {
            ServerMessage::Info {
                model, provider, ..
            } => Ok(ServerInfo { model, provider }),
            other => Err(unexpected(other, "info")),
        }
    }

    /// The session's context usage against the agent's context window.
    ///
    /// Pass the session id to have the server create/load that session before
    /// measuring (so a resumed conversation reports real numbers). While an
    /// ask is streaming the reply carries zeros — query when idle.
    pub async fn usage(&self, session_id: Option<&str>) -> Result<Usage> {
        let mut request = ClientRequest::usage(self.next_correlation_id());
        if let Some(session_id) = session_id {
            request = request.with_session(session_id);
        }
        match self.request(request).await? {
            ServerMessage::Usage {
                used_tokens,
                context_limit,
                ..
            } => Ok(Usage {
                used_tokens,
                context_limit,
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

    /// Close the connection, aborting any running ask. Other `Client`
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

    /// Start the ask and return its event stream.
    pub async fn send(self) -> Result<AskStream> {
        let request = ClientRequest::ask(self.client.next_correlation_id(), self.prompt);
        let request = ClientRequest {
            session_id: self.session_id,
            agent: self.agent,
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
