//! Fake in-process gewu server for tests: speaks the real protocol over
//! real TCP sockets but answers from scripted replies, so client behavior
//! is verified end-to-end without an LLM.

use std::collections::VecDeque;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// One scripted server behavior, consumed per incoming request line.
pub(crate) enum Step {
    /// Send these lines in response to the next request.
    Reply(Vec<String>),
    /// Send these lines, then close the connection (simulating a server
    /// crash mid-ask).
    ReplyThenClose(Vec<String>),
}

/// A running fake server bound to an OS-assigned port.
pub(crate) struct FakeServer {
    pub port: u16,
    shutdown: mpsc::Sender<()>,
}

impl FakeServer {
    /// Stop the server and wait for its accept loop to exit.
    pub async fn stop(self) {
        let _ = self.shutdown.send(()).await;
    }
}

/// Spawn a fake server; `script` builds a fresh queue per connection.
pub(crate) async fn spawn(
    script: impl Fn() -> VecDeque<Step> + Send + Sync + 'static,
) -> FakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    let script = std::sync::Arc::new(script);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break };
                    let script = script.clone();
                    tokio::spawn(async move {
                        handle_connection(stream, script()).await;
                    });
                }
            }
        }
    });

    FakeServer {
        port,
        shutdown: shutdown_tx,
    }
}

/// Handle one connection: for each line received, pop a step and act on it.
async fn handle_connection(stream: TcpStream, mut steps: VecDeque<Step>) {
    let (reader_half, mut writer_half) = stream.into_split();
    let mut lines = BufReader::new(reader_half).lines();

    loop {
        if lines.next_line().await.is_err() {
            break;
        }
        match steps.pop_front() {
            Some(Step::Reply(outs)) => {
                for out in outs {
                    if write_line(&mut writer_half, &out).await.is_err() {
                        return;
                    }
                }
            }
            Some(Step::ReplyThenClose(outs)) => {
                for out in outs {
                    let _ = write_line(&mut writer_half, &out).await;
                }
                break;
            }
            None => break,
        }
    }
}

async fn write_line(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    line: &str,
) -> std::io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

/// Convenience: a `ready` frame line for the given request id.
pub(crate) fn ready_line(id: &str, request_id: &str, session_id: &str) -> String {
    let id: serde_json::Value = serde_json::from_str(id).unwrap_or_else(|_| id.into());
    serde_json::json!({
        "type": "ready",
        "id": id,
        "request_id": request_id,
        "session_id": session_id,
        "agent": "build",
    })
    .to_string()
}

/// Convenience: an `event` frame line.
pub(crate) fn event_line(request_id: &str, event: serde_json::Value) -> String {
    serde_json::json!({
        "type": "event",
        "id": null,
        "request_id": request_id,
        "event": event,
    })
    .to_string()
}

/// Convenience: a `result` frame line.
pub(crate) fn result_line(request_id: &str, session_id: &str, response: &str) -> String {
    serde_json::json!({
        "type": "result",
        "id": null,
        "request_id": request_id,
        "session_id": session_id,
        "response": response,
    })
    .to_string()
}

/// Convenience: an `error` frame line addressed to `id` (or unaddressed).
pub(crate) fn error_line(id: Option<&str>, error: &str) -> String {
    let mut obj = serde_json::json!({ "type": "error", "error": error });
    if let Some(id) = id {
        obj["id"] = serde_json::from_str::<serde_json::Value>(id).unwrap_or_else(|_| id.into());
    }
    obj.to_string()
}

/// Convenience: a `pong` frame line.
pub(crate) fn pong_line(id: &str) -> String {
    let id: serde_json::Value = serde_json::from_str(id).unwrap_or_else(|_| id.into());
    serde_json::json!({ "type": "pong", "id": id }).to_string()
}

/// Convenience: an `info` frame line.
pub(crate) fn info_line(id: &str, model: &str, provider: &str) -> String {
    let id: serde_json::Value = serde_json::from_str(id).unwrap_or_else(|_| id.into());
    serde_json::json!({
        "type": "info",
        "id": id,
        "model": model,
        "provider": provider,
    })
    .to_string()
}

/// Convenience: a `deleted` frame line.
pub(crate) fn deleted_line(id: &str, session_id: &str) -> String {
    let id: serde_json::Value = serde_json::from_str(id).unwrap_or_else(|_| id.into());
    serde_json::json!({
        "type": "deleted",
        "id": id,
        "session_id": session_id,
    })
    .to_string()
}

/// Convenience: a `usage` frame line.
pub(crate) fn usage_line(id: &str, used_tokens: u64, context_limit: u64) -> String {
    let id: serde_json::Value = serde_json::from_str(id).unwrap_or_else(|_| id.into());
    serde_json::json!({
        "type": "usage",
        "id": id,
        "used_tokens": used_tokens,
        "context_limit": context_limit,
    })
    .to_string()
}

/// Convenience: a `tools` frame line.
pub(crate) fn tools_line(id: &str, tools: &[&str]) -> String {
    let id: serde_json::Value = serde_json::from_str(id).unwrap_or_else(|_| id.into());
    serde_json::json!({ "type": "tools", "id": id, "tools": tools }).to_string()
}

/// Convenience: an `agents` frame line.
pub(crate) fn agents_line(id: &str, agents: &[&str]) -> String {
    let id: serde_json::Value = serde_json::from_str(id).unwrap_or_else(|_| id.into());
    serde_json::json!({ "type": "agents", "id": id, "agents": agents }).to_string()
}

/// Convenience: a `sessions` frame line.
pub(crate) fn sessions_line(id: &str, sessions: serde_json::Value) -> String {
    let id: serde_json::Value = serde_json::from_str(id).unwrap_or_else(|_| id.into());
    serde_json::json!({ "type": "sessions", "id": id, "sessions": sessions }).to_string()
}

/// Convenience: a `session` frame line.
pub(crate) fn session_line(id: &str, session_id: &str, messages: serde_json::Value) -> String {
    let id: serde_json::Value = serde_json::from_str(id).unwrap_or_else(|_| id.into());
    serde_json::json!({
        "type": "session",
        "id": id,
        "session_id": session_id,
        "messages": messages,
    })
    .to_string()
}
