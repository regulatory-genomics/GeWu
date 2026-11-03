//! Transport connections (TCP with newline-delimited JSON, WebSocket with
//! one JSON message per text frame).

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message as WsMessage};

use crate::Error;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The write half of a connection.
pub(crate) enum ConnSink {
    Tcp(tokio::net::tcp::OwnedWriteHalf),
    Ws(futures::stream::SplitSink<WsStream, WsMessage>),
}

impl ConnSink {
    async fn send(&mut self, line: &str) -> Result<(), Error> {
        match self {
            ConnSink::Tcp(sink) => {
                sink.write_all(line.as_bytes())
                    .await
                    .map_err(|e| Error::Connection(e.to_string()))?;
                sink.write_all(b"\n")
                    .await
                    .map_err(|e| Error::Connection(e.to_string()))?;
                sink.flush()
                    .await
                    .map_err(|e| Error::Connection(e.to_string()))
            }
            ConnSink::Ws(sink) => sink
                .send(WsMessage::text(line))
                .await
                .map_err(|e| Error::Connection(e.to_string())),
        }
    }

    async fn close(&mut self) {
        match self {
            ConnSink::Tcp(sink) => {
                // Shut down the write half so the server sees EOF.
                let _ = sink.shutdown().await;
            }
            ConnSink::Ws(sink) => {
                // Politely send a WebSocket Close frame.
                let _ = sink.close().await;
            }
        }
    }
}

/// The read half of a connection.
pub(crate) enum ConnSource {
    Tcp(tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>),
    Ws(futures::stream::SplitStream<WsStream>),
}

impl ConnSource {
    /// Next message from the server; `None` when the connection closed.
    async fn next_message(&mut self) -> Option<Result<String, Error>> {
        match self {
            ConnSource::Tcp(lines) => match lines.next_line().await {
                Ok(Some(line)) => Some(Ok(line)),
                Ok(None) => None,
                Err(e) => Some(Err(Error::Connection(e.to_string()))),
            },
            ConnSource::Ws(source) => loop {
                match source.next().await {
                    Some(Ok(WsMessage::Text(text))) => break Some(Ok(text.to_string())),
                    // Close frames and anything non-text end the stream.
                    Some(Ok(WsMessage::Close(_))) | None => break None,
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => break Some(Err(Error::Connection(e.to_string()))),
                }
            },
        }
    }
}

/// A connection to a gewu server: one JSON message per line (TCP) or text
/// frame (WebSocket), in both directions.
pub(crate) struct Connection {
    sink: ConnSink,
    source: ConnSource,
}

impl Connection {
    /// Connect over TCP (newline-delimited JSON).
    pub async fn tcp(addr: &str) -> Result<Self, Error> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| Error::Connection(format!("failed to connect to {addr}: {e}")))?;
        let (read, write) = stream.into_split();
        Ok(Self {
            sink: ConnSink::Tcp(write),
            source: ConnSource::Tcp(BufReader::new(read).lines()),
        })
    }

    /// Connect over WebSocket (`ws://` or `wss://` URL).
    pub async fn websocket(url: &str) -> Result<Self, Error> {
        let (ws, _response) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| Error::Connection(format!("failed to connect to {url}: {e}")))?;
        let (sink, source) = ws.split();
        Ok(Self {
            sink: ConnSink::Ws(sink),
            source: ConnSource::Ws(source),
        })
    }

    pub(crate) async fn send(&mut self, line: &str) -> Result<(), Error> {
        self.sink.send(line).await
    }

    pub(crate) async fn next_message(&mut self) -> Option<Result<String, Error>> {
        self.source.next_message().await
    }

    pub(crate) async fn close(&mut self) {
        self.sink.close().await;
    }
}
