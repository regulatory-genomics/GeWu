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
    /// Token for the upstream gewu server, when it requires one.
    pub server_token: Option<String>,
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

    match GuiSession::new(&state.endpoint, state.server_token.as_deref()).await {
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
            server_token: None,
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
                "subscribe_session" => vec![
                    json!({"type":"ready", "id":id, "request_id":"", "session_id":"sess-1", "agent":"build"}),
                    json!({"type":"session_snapshot", "session_id":"sess-1", "messages":[{"role":"user", "content":[{"type":"text", "text":"run a tool"}]}], "frames":[
                        {"type":"event", "id":null, "request_id":"req-1", "event":{"type":"content_delta", "delta":"Working"}},
                        {"type":"event", "id":null, "request_id":"req-1", "event":{"type":"tool_start", "tool":"bash", "input":"seq 10"}},
                        {"type":"event", "id":null, "request_id":"req-1", "event":{"type":"tool_finish", "tool":"bash", "title":"seq 10", "output":"line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10"}},
                        {"type":"event", "id":null, "request_id":"req-1", "event":{"type":"content_delta", "delta":"hello world"}}
                    ], "busy":false, "seq":4}),
                ],
                "info" => vec![json!({
                    "type": "info", "id": id, "model": "fake-model", "provider": "fake",
                    "working_directory": "/tmp/fake-project",
                })],
                "list_models" => vec![json!({
                    "type": "models", "id": id,
                    "models": [
                        {"selector": "fake/one", "label": "fake-model"},
                        {"selector": "fake/two", "label": "fake-other"},
                    ],
                })],
                "switch_model" => vec![json!({
                    "type": "model_switched", "id": id, "session_id": "sess-1",
                    "model": request.get("model").and_then(Value::as_str).unwrap_or(""),
                    "label": "fake-other",
                })],
                "list_sessions" => vec![json!({
                    "type": "sessions", "id": id, "sessions": [],
                })],
                "list_tools" => vec![json!({
                    "type": "tools", "id": id, "tools": ["read", "grep"],
                })],
                "list_files" => vec![json!({
                    "type": "files", "id": id,
                    "path": request.get("path").and_then(Value::as_str).unwrap_or(""),
                    "entries": [],
                })],
                "write_file" => vec![json!({
                    "type": "file_written", "id": id,
                    "path": request.get("path").and_then(Value::as_str).unwrap_or(""),
                    "bytes": request.get("content_base64").and_then(Value::as_str)
                        .map(|payload| payload.len() * 3 / 4)
                        .unwrap_or(0),
                })],
                "ping" => vec![json!({"type": "pong", "id": id})],
                "usage" => vec![json!({
                    "type": "usage", "id": id, "used_tokens": 1000, "context_limit": 10000,
                })],
                "ask" => {
                    let request_id = "req-1".to_string();
                    let prompt = request.get("prompt").and_then(Value::as_str).unwrap_or("");
                    let mut replies = vec![json!({
                        "type": "ready", "id": id, "request_id": request_id,
                        "session_id": "sess-1", "agent": "build",
                    })];
                    // Prompts mentioning "think" exercise the thought-block
                    // path; "tool" the tool-card path.
                    if prompt.contains("think") {
                        replies.push(json!({
                            "type": "event", "id": null, "request_id": request_id,
                            "event": {"type": "reasoning_delta", "delta": "pondering "},
                        }));
                        replies.push(json!({
                            "type": "event", "id": null, "request_id": request_id,
                            "event": {"type": "reasoning_delta", "delta": "deeply"},
                        }));
                    }
                    if prompt.contains("tool") {
                        let output = (1..=10)
                            .map(|n| format!("line {n}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        // Text before the tool call exercises the assistant
                        // body swap in `tool_card_start`.
                        replies.push(json!({
                            "type": "event", "id": null, "request_id": request_id,
                            "event": {"type": "content_delta", "delta": "Working"},
                        }));
                        replies.push(json!({
                            "type": "event", "id": null, "request_id": request_id,
                            "event": {"type": "tool_start", "tool": "bash", "input": "seq 10"},
                        }));
                        replies.push(json!({
                            "type": "event", "id": null, "request_id": request_id,
                            "event": {
                                "type": "tool_finish", "tool": "bash",
                                "title": "seq 10", "output": output,
                            },
                        }));
                    }
                    if prompt.contains("rerun") {
                        // Two rounds of tool calls: the second round's tool
                        // block joins the first round's run, so its collapsed
                        // thought cannot ride on a tool_card_start swap.
                        for label in ["first", "second"] {
                            replies.push(json!({
                                "type": "event", "id": null, "request_id": request_id,
                                "event": {"type": "reasoning_delta", "delta": format!("pondering {label} ")},
                            }));
                            replies.push(json!({
                                "type": "event", "id": null, "request_id": request_id,
                                "event": {"type": "tool_start", "tool": "bash", "input": format!("seq {label}")},
                            }));
                            replies.push(json!({
                                "type": "event", "id": null, "request_id": request_id,
                                "event": {
                                    "type": "tool_finish", "tool": "bash",
                                    "title": format!("seq {label}"), "output": format!("{label} out"),
                                },
                            }));
                        }
                    }
                    replies.push(json!({
                        "type": "event", "id": null, "request_id": request_id,
                        "event": {"type": "content_delta", "delta": "hello "},
                    }));
                    // Pause past the GUI's streaming-Markdown throttle so the
                    // next delta is pushed as a full rendered body.
                    replies.push(json!({"__sleep_ms": 150}));
                    replies.push(json!({
                        "type": "event", "id": null, "request_id": request_id,
                        "event": {"type": "content_delta", "delta": "world"},
                    }));
                    // The agent reports usage at each turn boundary; the GUI
                    // must push it to the browser as a status update.
                    replies.push(json!({
                        "type": "event", "id": null, "request_id": request_id,
                        "event": {"type": "usage", "used_tokens": 2000, "context_limit": 10000},
                    }));
                    replies.push(json!({
                        "type": "result", "id": null, "request_id": request_id,
                        "session_id": "sess-1", "response": "hello world",
                    }));
                    replies
                }
                _ => Vec::new(),
            };
            for reply in replies {
                // Internal marker: pause before sending the next frame, to
                // exercise timing-dependent paths in the GUI.
                if let Some(ms) = reply.get("__sleep_ms").and_then(Value::as_u64) {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    continue;
                }
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
            server_token: None,
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
    async fn upload_files_refreshes_tree_and_notifies() {
        let upstream = spawn_fake_upstream().await;
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            token: None,
            server_token: None,
        })
        .await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let snapshot = next_json(&mut ws).await;
        assert_eq!(snapshot["type"], "snapshot");

        // A traversal name is sanitized client-side before it reaches the
        // upstream, so the reply echoes the bare name.
        ws.send(WsMessage::text(
            r#"{"op":"upload_files","files":[{"name":"../notes.bin","content_base64":"aGVsbG8="}]}"#,
        ))
        .await
        .unwrap();

        let tree = loop {
            let value = next_json(&mut ws).await;
            if value["type"] != "status" {
                break value;
            }
        };
        assert_eq!(tree["type"], "file_tree");
        assert!(tree["html"].as_str().unwrap().contains("data-tree-upload"));
        let notice = loop {
            let value = next_json(&mut ws).await;
            if value["type"] != "status" {
                break value;
            }
        };
        assert_eq!(notice["type"], "notice");
        assert!(
            notice["message"].as_str().unwrap().contains("notes.bin"),
            "{}",
            notice
        );
    }

    #[tokio::test]
    async fn upload_files_reports_invalid_names_as_errors() {
        let upstream = spawn_fake_upstream().await;
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            token: None,
            server_token: None,
        })
        .await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let snapshot = next_json(&mut ws).await;
        assert_eq!(snapshot["type"], "snapshot");

        ws.send(WsMessage::text(
            r#"{"op":"upload_files","files":[{"name":"..","content_base64":"aGVsbG8="}]}"#,
        ))
        .await
        .unwrap();

        let error = loop {
            let value = next_json(&mut ws).await;
            if value["type"] != "status" {
                break value;
            }
        };
        assert_eq!(error["type"], "error");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("not a valid file name"),
            "{}",
            error
        );
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
            server_token: None,
        })
        .await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();

        let snapshot = next_json(&mut ws).await;
        assert_eq!(snapshot["type"], "snapshot");
        assert_eq!(snapshot["active"], 1);
        assert!(snapshot["status"].as_str().unwrap().contains("fake-model"));
        let sidebar = snapshot["sidebar"].as_str().unwrap();
        assert!(sidebar.contains("<li>read</li>"), "{sidebar}");
        assert!(sidebar.contains("<li>grep</li>"), "{sidebar}");
        // The working directory appears once as the file tree root.
        assert!(
            sidebar
                .contains("class=\"tree-root\" title=\"/tmp/fake-project\">/tmp/fake-project</p>"),
            "{sidebar}"
        );

        ws.send(WsMessage::text(r#"{"op":"ask","tab":1,"prompt":"hello"}"#))
            .await
            .unwrap();

        let mut saw_streaming_text = false;
        let mut saw_user_block = false;
        let mut saw_delta = false;
        let mut final_transcript = false;
        let mut usage = false;
        let mut streamed_usage = false;
        let mut final_tabs = false;
        for _ in 0..60 {
            let value = next_json(&mut ws).await;
            match value["type"].as_str().unwrap_or("") {
                // Streaming updates arrive as targeted appends, not as full
                // transcript re-renders.
                "block_append" => {
                    let html = value["html"].as_str().unwrap();
                    if html.contains("class=\"block user\"") {
                        saw_user_block = true;
                    }
                    if html.contains("body markdown streaming") && html.contains("hello") {
                        saw_streaming_text = true;
                    }
                }
                "streaming_body" => {
                    assert_eq!(value["index"], 2);
                    assert!(
                        value["html"]
                            .as_str()
                            .unwrap()
                            .contains("<p>hello world</p>"),
                        "{}",
                        value["html"]
                    );
                    saw_delta = true;
                }
                "transcript" => {
                    let html = value["html"].as_str().unwrap();
                    if html.contains("<p>hello world</p>")
                        && !html.contains("body markdown streaming")
                    {
                        final_transcript = true;
                    }
                }
                "status" => {
                    let html = value["html"].as_str().unwrap();
                    if html.contains("1k / 10k (10%)") {
                        usage = true;
                    }
                    // Mid-ask usage from the agent's own `usage` event.
                    if html.contains("2k / 10k (20%)") {
                        streamed_usage = true;
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
            if saw_streaming_text
                && saw_user_block
                && saw_delta
                && final_transcript
                && usage
                && streamed_usage
                && final_tabs
            {
                break;
            }
        }
        assert!(saw_user_block, "the prompt should append as a user block");
        assert!(
            saw_streaming_text,
            "first delta should append a streaming Markdown block"
        );
        assert!(
            saw_delta,
            "later deltas should stream as rendered Markdown bodies"
        );
        assert!(
            final_transcript,
            "the final Markdown transcript should arrive"
        );
        assert!(usage, "context usage should be refreshed");
        assert!(
            streamed_usage,
            "mid-ask usage events should update the gauge in real time"
        );
        assert!(
            final_tabs,
            "the final tabs message should be idle with a session"
        );
    }

    #[tokio::test]
    async fn browser_can_switch_model_from_the_picker() {
        let upstream = spawn_fake_upstream().await;
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            token: None,
            server_token: None,
        })
        .await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();

        // The initial snapshot renders the catalog as a picker with the
        // server default selected.
        let snapshot = next_json(&mut ws).await;
        assert_eq!(snapshot["type"], "snapshot");
        let status = snapshot["status"].as_str().unwrap();
        assert!(status.contains("model-picker"), "{status}");
        assert!(
            status.contains("<option value=\"fake/one\" selected>fake-model</option>"),
            "{status}"
        );

        ws.send(WsMessage::text(
            r#"{"op":"switch_model","tab":1,"model":"fake/two"}"#,
        ))
        .await
        .unwrap();
        // Interim status updates (ping notes, usage refreshes) may precede
        // the switch's own status; drain until the new selection appears.
        let mut switched = None;
        for _ in 0..20 {
            let value = next_json(&mut ws).await;
            if value["type"] == "status" {
                let html = value["html"].as_str().unwrap();
                if html.contains("<option value=\"fake/two\" selected>fake-other</option>") {
                    switched = Some(html.to_string());
                    break;
                }
            }
        }
        assert!(
            switched.is_some(),
            "the switched model should be selected in a status update"
        );
    }

    #[tokio::test]
    async fn toggling_a_tool_sends_only_that_card() {
        let upstream = spawn_fake_upstream().await;
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            token: None,
            server_token: None,
        })
        .await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let snapshot = next_json(&mut ws).await;
        assert_eq!(snapshot["type"], "snapshot");

        ws.send(WsMessage::text(
            json!({"op": "ask", "tab": 1, "prompt": "run a tool"}).to_string(),
        ))
        .await
        .unwrap();

        let mut transcript = String::new();
        for _ in 0..80 {
            let value = next_json(&mut ws).await;
            match value["type"].as_str().unwrap_or("") {
                "transcript" => {
                    transcript = value["html"].as_str().unwrap_or("").to_string();
                }
                "tabs" => {
                    let tab = &value["tabs"][0];
                    if tab["session_id"] == "sess-1" && tab["busy"] == false {
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(transcript.contains("+4 lines"), "{transcript}");
        assert!(!transcript.contains("line 10"), "{transcript}");

        // Let the ask's remaining terminal messages (status/tabs) drain so the
        // only thing after the toggle is its own reply.
        while tokio::time::timeout(std::time::Duration::from_millis(150), next_json(&mut ws))
            .await
            .is_ok()
        {}

        // Take the block index straight from the rendered card.
        let marker = "data-action=\"toggle_tool\"";
        let tail = &transcript[transcript.find(marker).expect("tool card")..];
        let start = tail.find("data-block=\"").expect("block") + "data-block=\"".len();
        let end = start + tail[start..].find('"').expect("closing quote");
        let block: usize = tail[start..end].parse().expect("numeric block");

        ws.send(WsMessage::text(
            json!({"op": "toggle_tool", "tab": 1, "block": block}).to_string(),
        ))
        .await
        .unwrap();

        for _ in 0..20 {
            let value = next_json(&mut ws).await;
            match value["type"].as_str().unwrap_or("") {
                "tool_card" => {
                    assert_eq!(value["first"], block);
                    let html = value["html"].as_str().unwrap();
                    assert!(html.contains("line 10"), "{html}");
                    assert!(!html.contains("+4 lines"), "{html}");
                    return;
                }
                "transcript" => panic!("a toggle must not resend the whole transcript: {value}"),
                _ => {}
            }
        }
        panic!("expected a tool_card message");
    }

    /// A thought that ends because a second round of tool calls starts must
    /// collapse even though its tool block joins the previous run: the
    /// collapsed block needs its own settled-body swap.
    #[tokio::test]
    async fn second_round_tool_call_settles_the_rounds_thought() {
        let upstream = spawn_fake_upstream().await;
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            token: None,
            server_token: None,
        })
        .await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let snapshot = next_json(&mut ws).await;
        assert_eq!(snapshot["type"], "snapshot");

        ws.send(WsMessage::text(
            json!({"op": "ask", "tab": 1, "prompt": "rerun"}).to_string(),
        ))
        .await
        .unwrap();

        // The snapshot already fills the transcript; block indices are
        // derived from the first run's card. The second round's thought sits
        // directly after it and must be settled by a body swap even though
        // its tool block joins the same run.
        let mut settled = Vec::new();
        let mut second_thought = None;
        for _ in 0..120 {
            let value = next_json(&mut ws).await;
            match value["type"].as_str().unwrap_or("") {
                "tool_card_start" => {
                    // The first run starts right after the first thought, so
                    // its payload already carries the settled body.
                    let html = value["assistant"][1].as_str().expect("assistant html");
                    assert!(html.contains("Thought "), "{html}");
                    assert!(!html.contains("Thinking"), "{html}");
                    second_thought = Some(value["first"].as_u64().expect("run start") as usize + 1);
                }
                "streaming_body" => {
                    let index = value["index"].as_u64().expect("body index") as usize;
                    let html = value["html"].as_str().unwrap_or("").to_string();
                    if Some(index) == second_thought && html.contains("Thought ") {
                        assert!(!html.contains("Thinking"), "{html}");
                        settled.push(html);
                    }
                }
                "tabs" => {
                    let tab = &value["tabs"][0];
                    if tab["session_id"] == "sess-1" && tab["busy"] == false {
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(
            !settled.is_empty(),
            "the second round's collapsed thought must receive a settled body swap"
        );
    }

    #[tokio::test]
    async fn toggling_a_thought_sends_only_that_body() {
        let upstream = spawn_fake_upstream().await;
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            token: None,
            server_token: None,
        })
        .await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let snapshot = next_json(&mut ws).await;
        assert_eq!(snapshot["type"], "snapshot");

        ws.send(WsMessage::text(
            json!({"op": "ask", "tab": 1, "prompt": "think hard"}).to_string(),
        ))
        .await
        .unwrap();

        let mut transcript = String::new();
        for _ in 0..80 {
            let value = next_json(&mut ws).await;
            match value["type"].as_str().unwrap_or("") {
                "transcript" => transcript = value["html"].as_str().unwrap_or("").to_string(),
                "tabs" => {
                    let tab = &value["tabs"][0];
                    if tab["session_id"] == "sess-1" && tab["busy"] == false {
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(transcript.contains("toggle_thought"), "{transcript}");
        assert!(
            !transcript.contains("thought-body"),
            "reasoning must be collapsed by default: {transcript}"
        );

        // Let the ask's remaining terminal messages drain so the only thing
        // after the toggle is its own reply.
        while tokio::time::timeout(std::time::Duration::from_millis(150), next_json(&mut ws))
            .await
            .is_ok()
        {}

        // Take the block index straight from the rendered toggle.
        let marker = "data-action=\"toggle_thought\"";
        let tail = &transcript[transcript.find(marker).expect("thought toggle")..];
        let start = tail.find("data-block=\"").expect("block") + "data-block=\"".len();
        let end = start + tail[start..].find('"').expect("closing quote");
        let block: usize = tail[start..end].parse().expect("numeric block");

        ws.send(WsMessage::text(
            json!({"op": "toggle_thought", "tab": 1, "block": block}).to_string(),
        ))
        .await
        .unwrap();

        for _ in 0..20 {
            let value = next_json(&mut ws).await;
            match value["type"].as_str().unwrap_or("") {
                "streaming_body" => {
                    assert_eq!(value["index"], block);
                    let html = value["html"].as_str().unwrap();
                    assert!(html.contains("thought-body"), "{html}");
                    assert!(html.contains("pondering deeply"), "{html}");
                    return;
                }
                "transcript" => panic!("a toggle must not resend the whole transcript: {value}"),
                _ => {}
            }
        }
        panic!("expected a streaming_body message");
    }

    #[tokio::test]
    async fn tool_events_send_only_card_updates() {
        let upstream = spawn_fake_upstream().await;
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            token: None,
            server_token: None,
        })
        .await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let snapshot = next_json(&mut ws).await;
        assert_eq!(snapshot["type"], "snapshot");

        ws.send(WsMessage::text(
            json!({"op": "ask", "tab": 1, "prompt": "run a tool"}).to_string(),
        ))
        .await
        .unwrap();

        let mut saw_card_start = false;
        let mut saw_assistant_swap = false;
        let mut saw_card_fill = false;
        let mut transcripts = 0;
        for _ in 0..80 {
            let value = next_json(&mut ws).await;
            match value["type"].as_str().unwrap_or("") {
                "tool_card_start" => {
                    saw_card_start = true;
                    assert_eq!(value["first"], 3);
                    let html = value["html"].as_str().unwrap();
                    assert!(html.contains("data-card=\"3\""), "{html}");
                    assert!(html.contains("tool-spinner"), "{html}");
                    // The assistant block before the run swaps to its final
                    // Markdown form.
                    if let Some(assistant) = value["assistant"].as_array() {
                        assert_eq!(assistant[0], 2);
                        assert!(assistant[1].as_str().unwrap().contains(">Working<"));
                        saw_assistant_swap = true;
                    }
                }
                "tool_card" => {
                    saw_card_fill = true;
                    assert_eq!(value["first"], 3);
                    let html = value["html"].as_str().unwrap();
                    assert!(html.contains("+4 lines"), "{html}");
                }
                "transcript" => transcripts += 1,
                "tabs" => {
                    let tab = &value["tabs"][0];
                    if tab["session_id"] == "sess-1" && tab["busy"] == false {
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(saw_card_start, "a new tool run should append its card");
        assert!(
            saw_assistant_swap,
            "tool_card_start should carry the assistant body swap"
        );
        assert!(
            saw_card_fill,
            "the tool output should fill the card in place"
        );
        assert_eq!(
            transcripts, 1,
            "only the terminal frame may re-render the transcript"
        );
    }
}
