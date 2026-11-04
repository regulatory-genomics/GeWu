//! The HTTP/WebSocket surface of the web GUI.
//!
//! A single axum listener serves the embedded single-page app plus the JSON
//! configuration endpoint and the `/ws` connection that carries
//! [`ClientMsg`] and [`ServerMsg`] frames. When a token is configured, the
//! first WebSocket frame must be `{"op":"auth","token":"…"}`.

use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::time::timeout;
use tracing::{info, warn};

use crate::cli::Endpoint;
use crate::protocol::{ClientMsg, ServerMsg};
use crate::session::GuiSession;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_JS: &str = include_str!("../assets/app.js");
const APP_CSS: &str = include_str!("../assets/app.css");

/// Bundled fonts (SIL OFL 1.1; licenses in `assets/fonts/`).
const FONTS: &[(&str, &[u8])] = &[
    (
        "inter-var.woff2",
        include_bytes!("../assets/fonts/inter-var.woff2"),
    ),
    (
        "jetbrains-mono-400.woff2",
        include_bytes!("../assets/fonts/jetbrains-mono-400.woff2"),
    ),
    (
        "jetbrains-mono-500.woff2",
        include_bytes!("../assets/fonts/jetbrains-mono-500.woff2"),
    ),
];

/// Everything is same-origin; model-authored Markdown cannot run script.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
                   font-src 'self'; img-src 'self' https:; connect-src 'self' ws: wss:; \
                   base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// How long a client has to send its auth frame.
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared state for every request.
#[derive(Clone)]
pub struct AppState {
    /// Upstream server every browser session connects to.
    pub endpoint: Endpoint,
    /// Required browser token; `None` means loopback-only anonymous access.
    pub token: Option<String>,
}

/// Serve the GUI until the listener fails.
pub async fn serve(listener: TcpListener, state: AppState) -> anyhow::Result<()> {
    serve_router(listener, router(state)).await
}

async fn serve_router(listener: TcpListener, app: Router) -> anyhow::Result<()> {
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("GUI server failed: {e}"))
}

/// Build the router. Exposed for tests.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/app.css", get(app_css))
        .route("/fonts/{name}", get(font))
        .route("/api/config", get(config))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

#[derive(Serialize)]
struct ConfigResponse {
    server: String,
    auth_required: bool,
}

async fn index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Html(INDEX_HTML),
    )
}

async fn app_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        APP_JS,
    )
}

async fn app_css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        APP_CSS,
    )
}

async fn font(Path(name): Path<String>) -> Response {
    let Some((_, bytes)) = FONTS.iter().find(|(file, _)| *file == name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        *bytes,
    )
        .into_response()
}

async fn config(State(state): State<AppState>) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        server: state.endpoint.display(),
        auth_required: state.token.is_some(),
    })
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    match authenticate(&mut socket, state.token.as_deref()).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            warn!("browser websocket error during auth: {e}");
            return;
        }
    }

    match GuiSession::new(&state.endpoint).await {
        Ok(session) => {
            if let Err(e) = session.run(socket).await {
                warn!("browser session ended with an error: {e:#}");
            }
        }
        Err(e) => {
            warn!("could not start browser session: {e:#}");
            let msg = ServerMsg::Error {
                message: format!("{e:#}"),
            };
            let _ = send_json(&mut socket, &msg).await;
            let _ = socket.send(Message::Close(None)).await;
        }
    }
}

/// Enforce the shared token on the first frame. Returns whether the
/// connection may continue.
async fn authenticate(socket: &mut WebSocket, expected: Option<&str>) -> Result<bool, axum::Error> {
    let Some(expected) = expected else {
        return Ok(true);
    };

    let message = timeout(AUTH_TIMEOUT, socket.recv()).await;
    let message = match message {
        Ok(message) => message,
        Err(_) => {
            reject(socket, "authentication timed out").await?;
            return Ok(false);
        }
    };

    let supplied = match message {
        Some(Ok(Message::Text(text))) => serde_json::from_str::<ClientMsg>(text.as_str())
            .ok()
            .and_then(|msg| match msg {
                ClientMsg::Auth { token } => Some(token),
                _ => None,
            }),
        Some(Ok(Message::Close(_))) | None => return Ok(false),
        Some(Ok(_)) => None,
        Some(Err(_)) => return Ok(false),
    };

    if supplied
        .as_deref()
        .is_some_and(|token| tokens_equal(token, expected))
    {
        info!("browser authenticated");
        Ok(true)
    } else {
        reject(socket, "unauthorized").await?;
        Ok(false)
    }
}

async fn reject(socket: &mut WebSocket, message: &str) -> Result<(), axum::Error> {
    let msg = ServerMsg::Error {
        message: message.to_string(),
    };
    send_json(socket, &msg).await?;
    socket.send(Message::Close(None)).await?;
    Ok(())
}

async fn send_json(socket: &mut WebSocket, msg: &ServerMsg) -> Result<(), axum::Error> {
    let text = serde_json::to_string(msg).expect("server messages are serializable");
    socket.send(Message::Text(text.into())).await
}

/// Compare two secrets without an early exit on the first differing byte.
fn tokens_equal(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    fn test_state(token: Option<&str>) -> AppState {
        AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: 8421,
            },
            token: token.map(str::to_string),
        }
    }

    async fn spawn_gui(state: AppState) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            serve_router(listener, router(state)).await.unwrap();
        });
        addr
    }

    /// A scripted upstream server that answers the ops the GUI uses.
    async fn spawn_fake_upstream() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(handle_fake_connection(stream));
            }
        });
        addr
    }

    async fn handle_fake_connection(stream: tokio::net::TcpStream) {
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(request) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            let op = request.get("op").and_then(Value::as_str).unwrap_or("");
            let replies: Vec<Value> = match op {
                "info" => vec![json!({
                    "type": "info", "id": id, "model": "fake-model", "provider": "fake",
                })],
                "list_sessions" => vec![json!({
                    "type": "sessions", "id": id, "sessions": [],
                })],
                "ping" => vec![json!({"type": "pong", "id": id})],
                "usage" => vec![json!({
                    "type": "usage", "id": id, "used_tokens": 1000, "context_limit": 10000,
                })],
                "ask" => {
                    let request_id = "req-1".to_string();
                    vec![
                        json!({
                            "type": "ready", "id": id, "request_id": request_id,
                            "session_id": "sess-1", "agent": "build",
                        }),
                        json!({
                            "type": "event", "id": null, "request_id": request_id,
                            "event": {"type": "content_delta", "delta": "hello "},
                        }),
                        json!({
                            "type": "event", "id": null, "request_id": request_id,
                            "event": {"type": "content_delta", "delta": "world"},
                        }),
                        json!({
                            "type": "result", "id": null, "request_id": request_id,
                            "session_id": "sess-1", "response": "hello world",
                        }),
                    ]
                }
                _ => Vec::new(),
            };
            for reply in replies {
                let mut line = reply.to_string();
                line.push('\n');
                if writer.write_all(line.as_bytes()).await.is_err() {
                    return;
                }
            }
            if writer.flush().await.is_err() {
                return;
            }
        }
    }

    async fn next_json(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> Value {
        loop {
            let frame = ws.next().await.expect("websocket ended").unwrap();
            if let WsMessage::Text(text) = frame {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }

    #[test]
    fn token_comparison_is_byte_exact() {
        assert!(tokens_equal("secret", "secret"));
        assert!(!tokens_equal("secret", "secreT"));
        assert!(!tokens_equal("secret", "secret "));
        assert!(!tokens_equal("", "x"));
        assert!(tokens_equal("", ""));
    }

    #[test]
    fn router_builds() {
        // A smoke test: constructing the router must not panic.
        let _ = router(test_state(None));
    }

    #[tokio::test]
    async fn config_reports_auth_and_upstream() {
        let addr = spawn_gui(test_state(Some("secret"))).await;
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /api/config HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.contains("200 OK"), "{response}");
        assert!(response.contains("\"auth_required\":true"), "{response}");
        assert!(
            response.contains("\"server\":\"tcp://127.0.0.1:8421\""),
            "{response}"
        );
    }

    async fn http_get(addr: std::net::SocketAddr, path: &str) -> Vec<u8> {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn fonts_are_served_and_unknown_names_404() {
        let addr = spawn_gui(test_state(None)).await;
        for (name, bytes) in FONTS {
            let response = http_get(addr, &format!("/fonts/{name}")).await;
            let head = String::from_utf8_lossy(&response[..response.len().min(512)]);
            assert!(head.contains("200 OK"), "{name}: {head}");
            assert!(head.contains("content-type: font/woff2"), "{name}: {head}");
            assert!(response.ends_with(bytes), "{name}: body mismatch");
        }
        let response = http_get(addr, "/fonts/..%2Fapp.css").await;
        assert!(String::from_utf8_lossy(&response).contains("404"));
    }

    #[tokio::test]
    async fn websocket_rejects_a_wrong_token() {
        let addr = spawn_gui(test_state(Some("secret"))).await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        ws.send(WsMessage::text(r#"{"op":"auth","token":"wrong"}"#))
            .await
            .unwrap();

        let value = next_json(&mut ws).await;
        assert_eq!(value["type"], "error");
        assert_eq!(value["message"], "unauthorized");
    }

    #[tokio::test]
    async fn websocket_accepts_the_right_token() {
        let upstream = spawn_fake_upstream().await;
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            token: Some("secret".to_string()),
        })
        .await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        ws.send(WsMessage::text(r#"{"op":"auth","token":"secret"}"#))
            .await
            .unwrap();

        // Passing auth must lead to a normal session snapshot.
        let value = next_json(&mut ws).await;
        assert_eq!(value["type"], "snapshot");
    }

    #[tokio::test]
    async fn browser_ask_streams_and_finishes() {
        let upstream = spawn_fake_upstream().await;
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            token: None,
        })
        .await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();

        let snapshot = next_json(&mut ws).await;
        assert_eq!(snapshot["type"], "snapshot");
        assert_eq!(snapshot["active"], 1);
        assert!(
            snapshot["status"]
                .as_str()
                .unwrap()
                .contains("fake/fake-model")
        );

        ws.send(WsMessage::text(r#"{"op":"ask","tab":1,"prompt":"hello"}"#))
            .await
            .unwrap();

        let mut saw_streaming_text = false;
        let mut saw_delta = false;
        let mut final_transcript = false;
        let mut usage = false;
        let mut final_tabs = false;
        for _ in 0..60 {
            let value = next_json(&mut ws).await;
            match value["type"].as_str().unwrap_or("") {
                "transcript" => {
                    let html = value["html"].as_str().unwrap();
                    if html.contains("class=\"body streaming\"") && html.contains("hello") {
                        saw_streaming_text = true;
                    }
                    if html.contains("<p>hello world</p>") && !html.contains("streaming-text") {
                        final_transcript = true;
                    }
                }
                "text_delta" => {
                    assert_eq!(value["block"], 2);
                    assert_eq!(value["delta"], "world");
                    saw_delta = true;
                }
                "status" => {
                    let html = value["html"].as_str().unwrap();
                    if html.contains("1k / 10k (10%)") {
                        usage = true;
                    }
                }
                "tabs" => {
                    let tab = &value["tabs"][0];
                    if tab["label"] == "hello"
                        && tab["session_id"] == "sess-1"
                        && tab["busy"] == false
                    {
                        final_tabs = true;
                    }
                }
                _ => {}
            }
            if saw_streaming_text && saw_delta && final_transcript && usage && final_tabs {
                break;
            }
        }
        assert!(saw_streaming_text, "first delta should render a transcript");
        assert!(saw_delta, "later deltas should stream as text_delta");
        assert!(
            final_transcript,
            "the final Markdown transcript should arrive"
        );
        assert!(usage, "context usage should be refreshed");
        assert!(
            final_tabs,
            "the final tabs message should be idle with a session"
        );
    }
}
