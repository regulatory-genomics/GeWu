//! Connection actor: owns the socket, demuxes incoming frames, and serves
//! requests from [`Client`](crate::Client) handles.

use gewu_protocol::{ClientRequest, ServerMessage};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};
use tracing::debug;

use crate::Error;

/// A pending request waiting for its reply frame.
struct Pending {
    /// Where to deliver the reply.
    reply: oneshot::Sender<Result<ServerMessage, Error>>,
}

/// An in-flight `ask`: its request id plus a broadcast channel carrying the
/// streamed frames until a terminal one arrives.
struct ActiveAsk {
    request_id: String,
    persistent: bool,
    tx: tokio::sync::broadcast::Sender<Result<ServerMessage, Error>>,
}
/// Commands the [`Client`](crate::Client) sends to the actor.
pub(crate) enum Command {
    /// Send a request and wait for its single reply frame.
    Request {
        request: ClientRequest,
        reply: oneshot::Sender<Result<ServerMessage, Error>>,
    },
    /// Start an ask and return the stream of its frames.
    Ask {
        request: ClientRequest,
        stream: oneshot::Sender<Result<AskHandle, Error>>,
    },
    Subscribe {
        request: ClientRequest,
        stream: oneshot::Sender<Result<AskHandle, Error>>,
    },
    /// Query the tracked session id.
    GetSession {
        reply: oneshot::Sender<Option<String>>,
    },
    /// Close the connection.
    Close { ack: oneshot::Sender<()> },
}

/// Handle over one ask's frame stream.
#[derive(Debug)]
pub(crate) struct AskHandle {
    rx: tokio::sync::broadcast::Receiver<Result<ServerMessage, Error>>,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    persistent: bool,
}

impl AskHandle {
    fn new(
        tx: tokio::sync::broadcast::Sender<Result<ServerMessage, Error>>,
        persistent: bool,
    ) -> Self {
        Self {
            rx: tx.subscribe(),
            done: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            persistent,
        }
    }

    /// Next frame from the ask; `None` after the terminal frame.
    pub(crate) async fn next_frame(&mut self) -> Option<Result<ServerMessage, Error>> {
        if self.done.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        match self.rx.recv().await {
            Ok(frame) => {
                let terminal = !self.persistent
                    && (matches!(
                        frame.as_ref().ok(),
                        Some(m) if m.is_ask_terminal().is_some()
                    ) || matches!(
                        frame.as_ref().ok(),
                        Some(ServerMessage::SessionSnapshot { busy: false, .. })
                    ));
                if terminal {
                    self.done.store(true, std::sync::atomic::Ordering::Release);
                }
                Some(frame)
            }
            // All senders dropped without a terminal frame: the connection
            // closed mid-ask.
            Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
            // Lagged: resubscribe semantics would lose frames; surface it.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                self.done.store(true, std::sync::atomic::Ordering::Release);
                Some(Err(Error::Connection(format!(
                    "ask stream lagged behind by {} messages",
                    n
                ))))
            }
        }
    }
}

/// State machine for the actor.
pub(crate) struct Inner {
    conn: crate::connection::Connection,
    commands: mpsc::Receiver<Command>,
    /// Requests awaiting a single reply, keyed by serialized correlation id.
    pending: HashMap<String, Pending>,
    /// The one in-flight ask, if any.
    active_ask: Option<ActiveAsk>,
    /// Session id learned from `ready`/`result` frames, reported by
    /// [`Client::session_id`](crate::Client::session_id).
    session_id: Option<String>,
}

impl Inner {
    /// Create the actor state for a fresh connection.
    pub(crate) fn new(
        conn: crate::connection::Connection,
        commands: mpsc::Receiver<Command>,
    ) -> Self {
        Self {
            conn,
            commands,
            pending: HashMap::new(),
            active_ask: None,
            session_id: None,
        }
    }
}

/// Key used to match replies to pending requests. The key is type-tagged so
/// a numeric id `1` and a string id `"1"` never collide (the server echoes
/// the id verbatim, so same-type matching is exact).
fn key_for(id: &serde_json::Value) -> String {
    match id {
        serde_json::Value::String(s) => format!("s:{s}"),
        serde_json::Value::Number(n) => format!("n:{n}"),
        serde_json::Value::Null => "null".to_string(),
        other => format!("o:{other}"),
    }
}

impl Inner {
    /// Send a request, register the reply channel, and deliver the wire
    /// line. Returns `Err` if sending failed.
    async fn send_request(
        &mut self,
        request: ClientRequest,
        reply: oneshot::Sender<Result<ServerMessage, Error>>,
    ) -> Result<(), Error> {
        let line = request.to_line();
        let key = key_for(&request.id);
        if self.pending.contains_key(&key) {
            let _ = reply.send(Err(Error::Protocol(format!(
                "duplicate correlation id {}",
                key
            ))));
            return Ok(());
        }
        self.conn.send(&line).await?;
        self.pending.insert(key, Pending { reply });
        Ok(())
    }

    /// Route an incoming server frame.
    fn route(&mut self, msg: ServerMessage) {
        // Session-bearing frames also update the tracked session id.
        if let ServerMessage::Ready { session_id, .. } | ServerMessage::Result { session_id, .. } =
            &msg
        {
            self.session_id = Some(session_id.clone());
        }

        if let Some(id) = msg.correlation_id() {
            let key = key_for(id);
            if let Some(pending) = self.pending.remove(&key) {
                // An `interrupted` reply confirms the abort of the active
                // ask; the server does not emit a terminal frame for the
                // aborted ask itself, so end its stream here.
                let interrupted_active_ask = matches!(msg, ServerMessage::Interrupted { .. })
                    && self.active_ask.as_ref().is_some_and(|ask| !ask.persistent);
                let reply = match msg {
                    ServerMessage::Error { error, .. } => Err(Error::Server(error)),
                    msg => Ok(msg),
                };
                let _ = pending.reply.send(reply);
                if interrupted_active_ask && let Some(ask) = self.active_ask.take() {
                    let _ = ask.tx.send(Err(Error::Interrupted));
                    drop(ask.tx);
                }
                return;
            }
        }

        // Terminal frames end the in-flight ask: clear it so the next ask
        // is accepted and close the broadcast channel after the terminal
        // frame (the consumer sees it, then `None`).
        if msg.is_ask_terminal().is_some()
            || matches!(&msg, ServerMessage::SessionSnapshot { busy: false, .. })
        {
            if self.active_ask.as_ref().is_some_and(|ask| ask.persistent) {
                if let Some(ask) = &self.active_ask {
                    let _ = ask.tx.send(Ok(msg));
                }
                return;
            }
            if let Some(ask) = self.active_ask.take() {
                let _ = ask.tx.send(Ok(msg));
                drop(ask.tx);
                return;
            }
        }
        // No active ask: fall through to the unmatched-frame path.

        // Frames without a correlatable id belong to the in-flight ask.
        match &mut self.active_ask {
            Some(ask) => {
                // Sanity: the server tags ask frames with the ask's request id.
                if let ServerMessage::Event { request_id, .. } = &msg
                    && !ask.persistent
                {
                    debug_assert_eq!(
                        request_id, &ask.request_id,
                        "event request id should match the active ask"
                    );
                }
                let _ = ask.tx.send(Ok(msg));
            }
            None => {
                debug!("dropping unmatched server frame: {}", msg.kind());
            }
        }
    }

    /// The connection ended. Fail everything outstanding.
    fn shutdown(&mut self, error: Error) {
        for (_, pending) in self.pending.drain() {
            let _ = pending.reply.send(Err(error.clone()));
        }
        if let Some(ask) = self.active_ask.take() {
            // Deliver a final error so stream consumers observe the failure.
            let _ = ask.tx.send(Err(error.clone()));
            drop(ask.tx); // closes the channel for remaining receivers
        }
    }
}

/// Run the actor until the connection closes or a close command arrives.
pub(crate) async fn run(mut inner: Inner) {
    loop {
        // Prioritize commands, then poll the socket.
        tokio::select! {
            biased;
            command = inner.commands.recv() => {
                match command {
                    Some(Command::Request { request, reply }) => {
                        if let Err(e) = inner.send_request(request, reply).await {
                            inner.shutdown(e);
                            break;
                        }
                    }
                    Some(Command::Ask { request, stream }) => {
                        match inner.start_ask(request, false).await {
                            Ok(handle) => {
                                let _ = stream.send(Ok(handle));
                            }
                            // Busy is a client-side precondition; the
                            // connection stays usable for the caller.
                            Err(Error::Busy) => {
                                let _ = stream.send(Err(Error::Busy));
                            }
                            Err(e) => {
                                let _ = stream.send(Err(e));
                                inner.shutdown(Error::Connection("connection closed".to_string()));
                                break;
                            }
                        }
                    }
                    Some(Command::Subscribe { request, stream }) => {
                        match inner.start_ask(request, true).await {
                            Ok(handle) => { let _ = stream.send(Ok(handle)); }
                            Err(e) => { let _ = stream.send(Err(e)); }
                        }
                    }
                    Some(Command::GetSession { reply }) => {
                        let _ = reply.send(inner.session_id.clone());
                    }
                    Some(Command::Close { ack }) => {
                        inner.conn.close().await;
                        inner.shutdown(Error::Connection("connection closed".to_string()));
                        let _ = ack.send(());
                        break;
                    }
                    None => {
                        // All Client handles dropped: close the connection.
                        inner.conn.close().await;
                        inner.shutdown(Error::Connection("connection closed".to_string()));
                        break;
                    }
                }
            }
            frame = inner.conn.next_message() => {
                match frame {
                    Some(Ok(line)) => {
                        match serde_json::from_str::<ServerMessage>(&line) {
                            Ok(msg) => inner.route(msg),
                            Err(e) => debug!("ignoring malformed server line: {}", e),
                        }
                    }
                    Some(Err(e)) => {
                        inner.shutdown(e);
                        break;
                    }
                    None => {
                        inner.shutdown(Error::Connection("connection closed".to_string()));
                        break;
                    }
                }
            }
        }
    }
}

impl Inner {
    /// Begin an ask: send the request, await `ready`, and hand back a stream
    /// handle for the subsequent frames.
    async fn start_ask(
        &mut self,
        request: ClientRequest,
        persistent: bool,
    ) -> Result<AskHandle, Error> {
        if self.active_ask.is_some() {
            return Err(Error::Busy);
        }

        // Send the wire request first (do not register as pending: the
        // `ready` reply carries the correlation id but is consumed by the
        // ask stream itself).
        let line = request.to_line();
        self.conn.send(&line).await?;

        // Read frames until `ready` (the ask's own ack) or an error frame
        // addressed to this request id.
        loop {
            match self.conn.next_message().await {
                Some(Ok(frame_line)) => {
                    // Skip malformed frames (they are never the ask's reply).
                    let Ok(msg) = serde_json::from_str::<ServerMessage>(&frame_line) else {
                        continue;
                    };
                    // Only an error frame addressed to this ask (or any
                    // error, since nothing else can be in flight while
                    // awaiting `ready`) means the ask failed before starting.
                    match msg {
                        ServerMessage::Ready {
                            session_id,
                            request_id,
                            ..
                        } => {
                            self.session_id = Some(session_id);
                            let (tx, _) = tokio::sync::broadcast::channel(1024);
                            let handle = AskHandle::new(tx.clone(), persistent);
                            self.active_ask = Some(ActiveAsk {
                                request_id,
                                persistent,
                                tx,
                            });
                            return Ok(handle);
                        }
                        ServerMessage::Error { error, .. } => {
                            return Err(Error::Server(error));
                        }
                        other => {
                            return Err(Error::Protocol(format!(
                                "expected 'ready' after ask, got '{}'",
                                other.kind()
                            )));
                        }
                    }
                }
                Some(Err(e)) => return Err(e),
                None => return Err(Error::Connection("connection closed".to_string())),
            }
        }
    }
}
