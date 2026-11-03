//! Stream of frames from a running ask.

use crate::inner::AskHandle;
use crate::{Error, Result};
use gewu_protocol::{AgentEvent, ServerMessage};

/// Stream of frames from a running ask.
///
/// Yields [`ServerMessage::Event`] frames as the agent works, then exactly
/// one terminal frame: [`ServerMessage::Result`] on success or
/// [`ServerMessage::Error`] on failure. `next_frame` returns `None` after
/// the terminal frame.
///
/// Use [`AskStream::final_response`] when you only care about the outcome
/// and [`AskStream::events`] to iterate decoded [`gewu_protocol::AgentEvent`]s.
#[derive(Debug)]
pub struct AskStream {
    handle: AskHandle,
    /// Buffer for the terminal frame consumed by `final_response`.
    terminal: Option<Result<ServerMessage>>,
    done: bool,
}

impl AskStream {
    pub(crate) fn new(handle: AskHandle) -> Self {
        Self {
            handle,
            terminal: None,
            done: false,
        }
    }

    /// Next frame from the ask; `None` after the terminal frame.
    pub async fn next_frame(&mut self) -> Option<Result<ServerMessage>> {
        if self.done {
            return None;
        }
        if let Some(terminal) = self.terminal.take() {
            self.done = true;
            return Some(terminal);
        }
        self.handle.next_frame().await
    }

    /// Consume the stream, returning the agent's final response text.
    ///
    /// Non-terminal frames are drained and discarded. Fails with
    /// [`Error::Server`] if the ask ended with a terminal error frame.
    pub async fn final_response(mut self) -> Result<String> {
        let mut response = None;
        while let Some(frame) = self.next_frame().await {
            if let ServerMessage::Result { response: r, .. } = frame? {
                response = Some(r);
            }
        }
        response.ok_or_else(|| {
            Error::Connection("connection closed before the ask completed".to_string())
        })
    }

    /// Decode an `event` frame's payload into an [`AgentEvent`].
    ///
    /// Non-event frames (terminal `result`, etc.) decode to `None`.
    pub fn decode_event(frame: &ServerMessage) -> Option<Result<AgentEvent>> {
        match frame {
            ServerMessage::Event { event, .. } => Some(
                serde_json::from_value(event.clone())
                    .map_err(|e| Error::Protocol(format!("malformed agent event: {e}"))),
            ),
            _ => None,
        }
    }
}
