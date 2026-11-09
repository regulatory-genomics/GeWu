//! Transport connections (TCP with newline-delimited JSON, WebSocket with
//! one JSON message per text frame).

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncWriteExt, BufReader};
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
    Tcp(BufReader<tokio::net::tcp::OwnedReadHalf>),
    Ws(futures::stream::SplitStream<WsStream>),
}

/// Upper bound on a single server message. Server snapshots (full session
/// history) can be large, but a hostile/broken server must not be able to
/// balloon client memory without limit on an unterminated line.
const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;

/// Reads one newline-terminated line, enforcing [`MAX_LINE_BYTES`].
/// Returns `Ok(None)` on clean EOF at a message boundary.
async fn read_bounded_line(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> Result<Option<String>, Error> {
    use tokio::io::AsyncBufReadExt;
    let mut line: Vec<u8> = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|e| Error::Connection(e.to_string()))?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            return Err(Error::Connection(
                "connection closed in the middle of a message".to_string(),
            ));
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                line.extend_from_slice(&available[..pos]);
                let line_len = pos + 1;
                reader.consume(line_len);
                let text = String::from_utf8_lossy(&line).into_owned();
                return Ok(Some(text));
            }
            None => {
                let n = available.len();
                line.extend_from_slice(available);
                reader.consume(n);
                if line.len() > MAX_LINE_BYTES {
                    return Err(Error::Connection(format!(
                        "server message exceeds the {} MB line cap",
                        MAX_LINE_BYTES / (1024 * 1024)
                    )));
                }
            }
        }
    }
}

impl ConnSource {
    /// Next message from the server; `None` when the connection closed.
    async fn next_message(&mut self) -> Option<Result<String, Error>> {
        match self {
            ConnSource::Tcp(reader) => match read_bounded_line(reader).await {
                Ok(Some(line)) => Some(Ok(line)),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
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
            source: ConnSource::Tcp(BufReader::new(read)),
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
