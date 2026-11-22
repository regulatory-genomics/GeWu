//! The HTTP/WebSocket surface of the web GUI.
//!
//! A single axum listener serves the embedded single-page app plus the JSON
//! configuration endpoint, the `/ws` connection that carries [`ClientMsg`]
//! and [`ServerMsg`] frames, ticket-authorized `/download` streaming, and
//! ticket-authorized HTML previews under `/preview`.
//! When a token is configured, the first WebSocket frame must be
//! `{"op":"auth","token":"…"}`.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::time::timeout;
use tracing::{info, warn};

use crate::cli::Endpoint;
use crate::download::DownloadTickets;
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
///
/// `data:` is allowed for images only because tool cards inline `read_file`
/// results as data URLs. Markdown cannot use it: `safe_dest` rewrites
/// `data:` link and image destinations to `#`, and raw HTML is escaped.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
                   font-src 'self'; img-src 'self' https: data:; connect-src 'self' ws: wss:; \
                   base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// CSP for a previewed HTML page. `sandbox` without `allow-same-origin` gives
/// the document an opaque origin: its scripts run, but they cannot read the
/// GUI's `localStorage` token, reuse cookies, or call the authenticated socket.
/// No fetch directive is set, so pages may still load their own CDN assets.
const PREVIEW_CSP: &str =
    "sandbox allow-scripts allow-forms allow-modals allow-popups allow-downloads";

/// How long a client has to send its auth frame.
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared state for every request.
#[derive(Clone)]
pub struct AppState {
    /// Upstream server every browser session connects to.
    pub endpoint: Endpoint,
    /// Version shown in the session sidebar.
    pub version: String,
    /// Latest release notice from the background check, if any.
    pub updates: watch::Receiver<Option<gewu_update::Update>>,
    /// Required browser token; `None` means loopback-only anonymous access.
    pub token: Option<String>,
    /// Token for the upstream gewu server, when it requires one.
    pub server_token: Option<String>,
    /// Upstream HTTP endpoint used for file downloads.
    pub server_http: String,
    /// Client for streaming download responses from the upstream.
    pub http_client: reqwest::Client,
    /// Short-lived download tickets minted by browser sessions.
    pub downloads: Arc<DownloadTickets>,
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
        .route("/download/{ticket}", get(download).head(download_head))
        .route("/preview/{ticket}", get(preview))
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

async fn download(State(state): State<AppState>, Path(ticket): Path<String>) -> Response {
    proxy_download(state, ticket, false).await
}

async fn download_head(State(state): State<AppState>, Path(ticket): Path<String>) -> Response {
    proxy_download(state, ticket, true).await
}

/// Stream one ticket-authorized download from the upstream HTTP endpoint.
async fn proxy_download(state: AppState, ticket: String, head_only: bool) -> Response {
    let Ok(id) = uuid::Uuid::parse_str(&ticket) else {
        return (StatusCode::NOT_FOUND, "download link is invalid or expired").into_response();
    };
    let Some((session_id, path)) = state.downloads.get(&id) else {
        return (StatusCode::NOT_FOUND, "download link is invalid or expired").into_response();
    };

    let url = format!("{}/download", state.server_http.trim_end_matches('/'));
    let request = if head_only {
        state.http_client.head(&url)
    } else {
        state.http_client.get(&url)
    };
    let mut request =
        request.query(&[("session_id", session_id.as_str()), ("path", path.as_str())]);
    if let Some(token) = state.server_token.as_deref() {
        request = request.bearer_auth(token);
    }
    let upstream = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            return (StatusCode::BAD_GATEWAY, format!("download failed: {error}")).into_response();
        }
    };
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if !upstream.status().is_success() {
        let message = upstream
            .text()
            .await
            .unwrap_or_else(|_| "download failed".to_string());
        return (status, message).into_response();
    }

    let mut builder = Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, "no-store");
    for name in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CONTENT_DISPOSITION,
        header::X_CONTENT_TYPE_OPTIONS,
    ] {
        if let Some(value) = upstream.headers().get(&name) {
            builder = builder.header(name, value);
        }
    }
    let body = if head_only {
        Body::empty()
    } else {
        Body::from_stream(upstream.bytes_stream())
    };
    builder.body(body).unwrap_or_else(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("download response failed: {error}"),
        )
            .into_response()
    })
}

async fn preview(State(state): State<AppState>, Path(ticket): Path<String>) -> Response {
    let Ok(id) = uuid::Uuid::parse_str(&ticket) else {
        return (StatusCode::NOT_FOUND, "preview link is invalid or expired").into_response();
    };
    let Some((session_id, path)) = state.downloads.get_preview(&id) else {
        return (StatusCode::NOT_FOUND, "preview link is invalid or expired").into_response();
    };

    let url = format!("{}/download", state.server_http.trim_end_matches('/'));
    let mut request = state
        .http_client
        .get(&url)
        .query(&[("session_id", session_id.as_str()), ("path", path.as_str())]);
    if let Some(token) = state.server_token.as_deref() {
        request = request.bearer_auth(token);
    }
    let upstream = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            return (StatusCode::BAD_GATEWAY, format!("preview failed: {error}")).into_response();
        }
    };
    if !upstream.status().is_success() {
        let status = StatusCode::from_u16(upstream.status().as_u16())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let message = upstream
            .text()
            .await
            .unwrap_or_else(|_| "preview failed".to_string());
        return (status, message).into_response();
    }

    // The upstream serves every file as an attachment with an opaque type;
    // override both so the browser renders the page instead of downloading it.
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CONTENT_SECURITY_POLICY, PREVIEW_CSP)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::REFERRER_POLICY, "no-referrer");
    if let Some(length) = upstream.headers().get(header::CONTENT_LENGTH) {
        builder = builder.header(header::CONTENT_LENGTH, length);
    }
    builder
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("preview response failed: {error}"),
            )
                .into_response()
        })
}

#[derive(serde::Deserialize)]
struct BrowserFocus {
    chat: Option<String>,
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    axum::extract::Query(focus): axum::extract::Query<BrowserFocus>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state, focus.chat))
}

async fn handle_socket(mut socket: WebSocket, state: AppState, chat: Option<String>) {
    match authenticate(&mut socket, state.token.as_deref()).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            warn!("browser websocket error during auth: {e}");
            return;
        }
    }

    match GuiSession::new(
        &state.endpoint,
        state.server_token.as_deref(),
        &state.version,
        state.updates.clone(),
        state.downloads.clone(),
    )
    .await
    {
        Ok(mut session) => {
            if let Some(resume) = chat {
                session.focus_browser_chat(&resume);
            }
            if let Err(e) = session.run(socket).await {
                warn!("browser session ended with an error: {e:#}");
            }
        }
        Err(e) => {
            warn!("could not start browser session: {e:#}");
            let msg = ServerMsg::Error {
                message: format!("{e:#}"),
                key: None,
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
        key: None,
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
            version: "0.0.0-test".to_string(),
            updates: watch::channel(None).1,
            server_http: "http://127.0.0.1:8422".to_string(),
            http_client: reqwest::Client::new(),
            downloads: Arc::new(DownloadTickets::default()),
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
        spawn_recorded_fake_upstream().await.0
    }

    async fn spawn_recorded_fake_upstream() -> (
        std::net::SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(handle_fake_connection(stream, tx.clone(), None, false));
            }
        });
        (addr, rx)
    }

    /// A recorded fake upstream whose live connections can be dropped the
    /// way an agent-server restart drops its clients' sockets.
    struct DroppableUpstream {
        port: u16,
        connections: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    }

    impl DroppableUpstream {
        fn port(&self) -> u16 {
            self.port
        }

        /// Abort every live connection task, closing their sockets.
        async fn drop_connections(&self) {
            let tasks: Vec<_> = self
                .connections
                .lock()
                .expect("connection list poisoned")
                .drain(..)
                .collect();
            for task in tasks {
                task.abort();
            }
            // Give the sockets a moment to close so client actors observe
            // EOF before the GUI reconnects.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn spawn_droppable_fake_upstream() -> DroppableUpstream {
        let (tx, _ops) = tokio::sync::mpsc::unbounded_channel();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let tracked = Arc::clone(&connections);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let tx = tx.clone();
                let handle = tokio::spawn(handle_fake_connection(stream, tx, None, false));
                tracked
                    .lock()
                    .expect("connection list poisoned")
                    .push(handle);
            }
        });
        DroppableUpstream { port, connections }
    }

    async fn handle_fake_connection(
        stream: tokio::net::TcpStream,
        ops: tokio::sync::mpsc::UnboundedSender<String>,
        records: Option<tokio::sync::mpsc::UnboundedSender<Value>>,
        saved: bool,
    ) {
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let mut bound = false;
        let mut workspace = "cwd".to_string();
        let mut agent = "build".to_string();
        let mut model = "fake/one".to_string();
        let mut effort = Value::Null;
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(request) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            let op = request.get("op").and_then(Value::as_str).unwrap_or("");
            let _ = ops.send(op.to_string());
            if !bound && request["session_id"].is_string() {
                bound = true;
                if saved {
                    workspace = "tfseq".into();
                }
            }
            match op {
                "switch_workspace" => {
                    bound = true;
                    workspace = request["workspace"].as_str().unwrap().into();
                }
                "switch_agent" => {
                    bound = true;
                    agent = request["target_agent"].as_str().unwrap().into();
                }
                "switch_model" => {
                    bound = true;
                    model = request["model"].as_str().unwrap().into();
                }
                "switch_reasoning_effort" => {
                    bound = true;
                    effort = request["effort"].clone();
                }
                "ask" => bound = true,
                _ => {}
            }
            if let Some(records) = &records {
                let _ = records.send(json!({"request":request,"bound":bound,"workspace":workspace,"agent":agent,"model":model,"effort":effort}));
            }
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
                    "workspaces": [{"id":"cwd","path":"/tmp/fake-project"},{"id":"tfseq","path":"/canonical/TFSeq"}],
                    "active_workspace": if saved && request["session_id"].is_string() { "tfseq" } else { &workspace },
                })],
                "list_models" => vec![json!({
                    "type": "models", "id": id,
                    "models": [
                        {"selector": "fake/one", "label": "fake-model", "supports_reasoning_effort": true, "default_effort": "high"},
                        {"selector": "fake/two", "label": "fake-other", "supports_reasoning_effort": false},
                    ],
                })],
                "switch_model" => vec![json!({
                    "type": "model_switched", "id": id, "session_id": "sess-1",
                    "model": request.get("model").and_then(Value::as_str).unwrap_or(""),
                    "label": "fake-other",
                })],
                "switch_workspace" => vec![json!({
                    "type": "workspace_switched", "id": id, "session_id": "sess-1",
                    "workspace": {"id": request["workspace"], "path": if workspace.is_empty() { "" } else if workspace == "cwd" { "/tmp/fake-project" } else { "/canonical/TFSeq" }},
                })],
                "switch_agent" => vec![json!({
                    "type":"agent_switched", "id":id, "session_id":"sess-1", "agent":agent, "tools":[],
                })],
                "switch_reasoning_effort" => vec![json!({
                    "type": "reasoning_effort_switched", "id": id, "session_id": "sess-1",
                    "effort": request.get("effort").cloned().unwrap_or(Value::Null),
                })],
                "list_sessions" => vec![json!({
                    "type": "sessions", "id": id, "sessions": if saved { vec![json!({"session_id":"sess-1","agent":"build","message_count":0,"updated_at":"2026-01-01"})] } else { vec![] },
                })],
                "get_session" => {
                    vec![json!({"type":"session","id":id,"session_id":"sess-1","messages":[]})]
                }
                "list_tools" => vec![json!({
                    "type": "tools", "id": id, "tools": ["read", "grep"],
                })],
                "list_files" => vec![json!({
                    "type": "files", "id": id,
                    "path": request.get("path").and_then(Value::as_str).unwrap_or(""),
                    "entries": if records.is_some() { vec![json!({"name":format!("{workspace}.txt"),"is_dir":false,"size":1})] } else { vec![] },
                })],
                "browse_workspace" => vec![json!({
                    "type": "directories", "id": id,
                    "path": request.get("path").cloned().unwrap_or(Value::Null),
                    "parent": Value::Null,
                    "entries": [],
                })],
                "read_file" => vec![
                    json!({"type":"file_content","id":id,"path":request["path"],"content":workspace,"truncated":false}),
                ],
                "write_file" => vec![json!({
                    "type": "file_written", "id": id,
                    "path": request.get("path").and_then(Value::as_str).unwrap_or(""),
                    "bytes": request.get("content_base64").and_then(Value::as_str)
                        .map(|payload| payload.len() * 3 / 4)
                        .unwrap_or(0),
                })],
                "interrupt" => {
                    vec![json!({"type": "interrupted", "id": id, "request_id": "req-1"})]
                }
                "delete_session" => vec![
                    json!({"type":"error", "id":id, "error":"session deletion forbidden by test upstream"}),
                ],
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
                    if prompt.contains("show image") {
                        replies.push(json!({
                            "type": "event", "id": null, "request_id": request_id,
                            "event": {"type": "tool_start", "tool": "read_file", "input": "shot.png"},
                        }));
                        replies.push(json!({
                            "type": "event", "id": null, "request_id": request_id,
                            "event": {
                                "type": "tool_finish", "tool": "read_file", "title": "read_file",
                                "output": json!({
                                    "type": "media", "mime": "image/png",
                                    "data": "ZmFrZQ==", "message": "Image read successfully",
                                }).to_string(),
                            },
                        }));
                    }
                    if prompt.contains("question") {
                        // A pending form must flip the status line to its
                        // waiting phase; the terminal result then dismisses it.
                        replies.push(json!({
                            "type": "event", "id": null, "request_id": request_id,
                            "event": {"type": "tool_start", "tool": "question", "input": "{}"},
                        }));
                        replies.push(json!({
                            "type": "event", "id": null, "request_id": request_id,
                            "event": {
                                "type": "question_asked", "request_id": "que_1",
                                "questions": [{
                                    "question": "Which database?",
                                    "header": "Database",
                                    "options": [{"label": "SQLite"}, {"label": "Postgres"}],
                                }],
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

    async fn choose_test_workspace(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tab: u64,
    ) {
        ws.send(WsMessage::text(
            json!({"op":"switch_workspace","tab":tab,"workspace":"cwd"}).to_string(),
        ))
        .await
        .unwrap();
        loop {
            let msg = next_json(ws).await;
            assert_ne!(msg["type"], "error");
            if msg["type"] == "file_tree" {
                break;
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

    async fn check_workspace_before_first_ask(config: Option<Value>, saved: bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (records, mut requests) = tokio::sync::mpsc::unbounded_channel();
            let (ops, _rx) = tokio::sync::mpsc::unbounded_channel();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let upstream = listener.local_addr().unwrap();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(handle_fake_connection(stream, ops.clone(), Some(records.clone()), saved));
                }
            });
            let addr = spawn_gui(AppState {
                endpoint: Endpoint::Tcp { host: "127.0.0.1".into(), port: upstream.port() },
                ..test_state(None)
            }).await;
            let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
            let snapshot = next_json(&mut ws).await;
            assert_eq!(snapshot["type"], "snapshot");
            let mut tab = snapshot["active"].as_u64().unwrap();
            if !saved {
                assert!(snapshot["tabs"][0]["session_id"].is_null());
                ws.send(WsMessage::text(json!({"op":"create_tab","workspace":"tfseq","path":"/alias/TFSeq"}).to_string())).await.unwrap();
                loop {
                    let value = next_json(&mut ws).await;
                    assert_ne!(value["type"], "error", "{value}");
                    if value["type"] == "tabs" {
                        tab = value["active"].as_u64().unwrap();
                    }
                    if value["type"] == "file_tree" {
                        assert!(value["html"].as_str().unwrap().contains("tfseq.txt"), "{value}");
                        assert!(value["html"].as_str().unwrap().contains("/canonical/TFSeq"));
                        assert!(!value["html"].as_str().unwrap().contains("cwd.txt"));
                        break;
                    }
                }
            }
            if let Some(mut config) = config.clone() {
                config["tab"] = json!(tab);
                ws.send(WsMessage::text(config.to_string())).await.unwrap();
            }
            for message in [json!({"op":"read_file","path":"tfseq.txt"}), json!({"op":"upload_files","files":[{"name":"upload.txt","content_base64":"eA=="}]}), json!({"op":"ask","tab":tab,"prompt":"pwd"})] {
                ws.send(WsMessage::text(message.to_string())).await.unwrap();
            }
            let mut seen = Vec::new();
            loop {
                let record = requests.recv().await.unwrap();
                let op = record["request"]["op"].as_str().unwrap();
                if matches!(op, "switch_agent" | "switch_model" | "switch_reasoning_effort" | "read_file" | "write_file" | "ask") || (op == "list_files" && record["bound"] == true) {
                    assert_eq!(record["workspace"], "tfseq", "{record}");
                    assert_eq!(record["bound"], true, "{record}");
                }
                let done = op == "ask";
                seen.push(record);
                if done { break; }
            }
            let ask = seen.last().unwrap();
            if let Some(config) = config {
                let op = config["op"].as_str().unwrap();
                assert!(seen.iter().any(|r| r["request"]["op"] == op));
                for field in ["agent", "model", "effort"] {
                    if !config[field].is_null() { assert_eq!(ask[field], config[field]); }
                }
            }
            assert!(seen.iter().any(|r| r["request"]["op"] == "read_file"));
            assert!(seen.iter().any(|r| r["request"]["op"] == "write_file"));
            assert_eq!(seen.iter().filter(|r| r["request"]["op"] == "switch_workspace").count(), usize::from(!saved));
            assert_eq!(ask["request"]["session_id"], "sess-1");
            ws.close(None).await.unwrap();
        }).await.expect("workspace initialization must complete");
    }

    #[tokio::test]
    async fn nondefault_workspace_mode_before_first_ask() {
        check_workspace_before_first_ask(Some(json!({"op":"switch_agent","agent":"plan"})), false)
            .await;
    }

    #[tokio::test]
    async fn nondefault_workspace_model_before_first_ask() {
        check_workspace_before_first_ask(
            Some(json!({"op":"switch_model","model":"fake/two"})),
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn nondefault_workspace_effort_before_first_ask() {
        check_workspace_before_first_ask(
            Some(json!({"op":"switch_reasoning_effort","effort":"max"})),
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn nondefault_workspace_files_before_first_ask() {
        check_workspace_before_first_ask(None, false).await;
    }

    #[tokio::test]
    async fn saved_workspace_is_bound_without_switching() {
        check_workspace_before_first_ask(
            Some(json!({"op":"switch_model","model":"fake/two"})),
            true,
        )
        .await;
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

    /// A tiny HTTP upstream that records requests and answers `/download`.
    async fn spawn_http_upstream(
        status: &'static str,
        body: &'static str,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        let Ok(read) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..read]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let request = String::from_utf8_lossy(&request).into_owned();
                    let head_only = request.starts_with("HEAD ");
                    let _ = tx.send(request);
                    let headers = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/octet-stream\r\n\
                         content-disposition: attachment; filename=\"note.txt\"\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let response = if head_only {
                        headers
                    } else {
                        format!("{headers}{body}")
                    };
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (addr, rx)
    }

    fn download_client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn streams_ticketed_downloads_from_the_http_endpoint() {
        let (upstream, mut requests) = spawn_http_upstream("200 OK", "hello from upstream").await;
        let state = AppState {
            server_http: format!("http://{upstream}"),
            server_token: Some("upstream-secret".to_string()),
            ..test_state(None)
        };
        let id = state
            .downloads
            .mint("sess-1".to_string(), "notes/a note.txt".to_string());
        let addr = spawn_gui(state).await;
        let client = download_client();

        let response = client
            .get(format!("http://{addr}/download/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"note.txt\""
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.text().await.unwrap(), "hello from upstream");

        let request = requests.recv().await.unwrap().to_lowercase();
        assert!(request.starts_with("get /download?"), "{request}");
        assert!(
            request.contains("authorization: bearer upstream-secret"),
            "{request}"
        );
        assert!(request.contains("session_id=sess-1"), "{request}");
        assert!(request.contains("path=notes%2fa"), "{request}");

        let response = client
            .head(format!("http://{addr}/download/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert!(response.text().await.unwrap().is_empty());
        let request = requests.recv().await.unwrap().to_lowercase();
        assert!(request.starts_with("head /download?"), "{request}");
    }

    #[tokio::test]
    async fn rejects_unknown_download_tickets() {
        let addr = spawn_gui(test_state(None)).await;
        let response = download_client()
            .get(format!("http://{addr}/download/{}", uuid::Uuid::new_v4()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn streams_sandboxed_html_previews_from_the_http_endpoint() {
        let (upstream, mut requests) = spawn_http_upstream("200 OK", "<h1>hi</h1>").await;
        let state = AppState {
            server_http: format!("http://{upstream}"),
            server_token: Some("upstream-secret".to_string()),
            ..test_state(None)
        };
        let id = state
            .downloads
            .mint_preview("sess-1".to_string(), "docs/a.html".to_string());
        let addr = spawn_gui(state).await;
        let response = download_client()
            .get(format!("http://{addr}/preview/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap();
        assert!(csp.starts_with("sandbox "), "{csp}");
        assert!(!csp.contains("allow-same-origin"), "{csp}");
        assert!(
            !response.headers().contains_key(header::CONTENT_DISPOSITION),
            "previews must render, not download"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.text().await.unwrap(), "<h1>hi</h1>");

        let request = requests.recv().await.unwrap().to_lowercase();
        assert!(request.starts_with("get /download?"), "{request}");
        assert!(
            request.contains("authorization: bearer upstream-secret"),
            "{request}"
        );
        assert!(request.contains("session_id=sess-1"), "{request}");
        assert!(request.contains("path=docs%2fa.html"), "{request}");
    }

    #[tokio::test]
    async fn preview_routes_reject_other_tickets() {
        let state = test_state(None);
        let download = state
            .downloads
            .mint("sess-1".to_string(), "docs/a.html".to_string());
        let addr = spawn_gui(state).await;
        let client = download_client();

        let response = client
            .get(format!("http://{addr}/preview/{download}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        assert_eq!(
            response.text().await.unwrap(),
            "preview link is invalid or expired"
        );

        let response = client
            .get(format!("http://{addr}/preview/{}", uuid::Uuid::new_v4()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn forwards_upstream_download_failures() {
        let (upstream, _requests) = spawn_http_upstream("404 Not Found", "no such file").await;
        let state = AppState {
            server_http: format!("http://{upstream}"),
            ..test_state(None)
        };
        let id = state
            .downloads
            .mint("sess-1".to_string(), "missing.txt".to_string());
        let addr = spawn_gui(state).await;
        let response = download_client()
            .get(format!("http://{addr}/download/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        assert_eq!(response.text().await.unwrap(), "no such file");
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
            ..test_state(Some("secret"))
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

    /// Regression test for GH issue #15: after the upstream server restarts
    /// (its live sockets drop), the GUI must replace its cached clients and
    /// re-observe instead of answering every request with
    /// "connection error: client is closed".
    #[tokio::test]
    async fn gui_reconnects_after_upstream_restart() {
        timeout(Duration::from_secs(15), async {
            let upstream = spawn_droppable_fake_upstream().await;
            let addr = spawn_gui(AppState {
                endpoint: Endpoint::Tcp {
                    host: "127.0.0.1".into(),
                    port: upstream.port(),
                },
                ..test_state(None)
            })
            .await;
            let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .unwrap();
            let initial = next_json(&mut ws).await["active"].as_u64().unwrap();

            // Bind the tab's upstream session so it has a cached client.
            ws.send(WsMessage::text(
                json!({"op":"ask","tab":initial,"prompt":"hello"}).to_string(),
            ))
            .await
            .unwrap();
            loop {
                let value = next_json(&mut ws).await;
                if value["type"] == "tabs"
                    && value["tabs"][0]["session_id"] == "sess-1"
                    && value["tabs"][0]["busy"] == false
                {
                    break;
                }
            }

            // Simulate the agent server restarting: every live socket drops.
            upstream.drop_connections().await;

            // Workspace browsing rides the admin connection; it must heal.
            ws.send(WsMessage::text(r#"{"op":"browse_workspace","path":""}"#))
                .await
                .unwrap();
            loop {
                let value = next_json(&mut ws).await;
                assert_ne!(
                    value["type"], "error",
                    "browse after upstream restart failed: {value}"
                );
                if value["type"] == "workspace_browser" {
                    break;
                }
            }

            // A new ask must reconnect the tab client and re-observe.
            ws.send(WsMessage::text(
                json!({"op":"ask","tab":initial,"prompt":"hello"}).to_string(),
            ))
            .await
            .unwrap();
            loop {
                let value = next_json(&mut ws).await;
                assert_ne!(
                    value["type"], "error",
                    "ask after upstream restart failed: {value}"
                );
                if value["type"] == "tabs"
                    && value["tabs"][0]["session_id"] == "sess-1"
                    && value["tabs"][0]["busy"] == false
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn workspace_close_preserves_sessions_and_reopens_groups() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (upstream, mut ops) = spawn_recorded_fake_upstream().await;
            let addr = spawn_gui(AppState {
                endpoint: Endpoint::Tcp {
                    host: "127.0.0.1".into(),
                    port: upstream.port(),
                },
                ..test_state(None)
            })
            .await;
            let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .unwrap();
            let initial = next_json(&mut ws).await["active"].as_u64().unwrap();
            choose_test_workspace(&mut ws, initial).await;
            ws.send(WsMessage::text(
                json!({"op":"ask","tab":initial,"prompt":"hello"}).to_string(),
            ))
            .await
            .unwrap();
            loop {
                let value = next_json(&mut ws).await;
                if value["type"] == "tabs"
                    && value["tabs"][0]["session_id"] == "sess-1"
                    && value["tabs"][0]["busy"] == false
                {
                    break;
                }
            }
            ws.send(WsMessage::text(
                r#"{"op":"close_workspace","workspace":"cwd"}"#,
            ))
            .await
            .unwrap();
            loop {
                let value = next_json(&mut ws).await;
                assert_ne!(value["type"], "error");
                if value["type"] == "tabs" {
                    assert_eq!(value["tabs"].as_array().unwrap().len(), 1);
                    assert_ne!(value["active"].as_u64().unwrap(), initial);
                    assert_eq!(value["tabs"][0]["workspace"], "");
                    assert!(value["tabs"][0]["session_id"].is_null());
                    break;
                }
            }
            let sidebar = next_json(&mut ws).await;
            assert_eq!(sidebar["type"], "session_sidebar");
            assert!(
                !sidebar["html"]
                    .as_str()
                    .unwrap()
                    .contains("data-session-workspace-group=\"cwd\"")
            );
            for op in ["create_tab", "close_workspace", "create_tab"] {
                ws.send(WsMessage::text(
                    json!({"op":op,"workspace":"runtime:/tmp/other","path":"/tmp/other"})
                        .to_string(),
                ))
                .await
                .unwrap();
                loop {
                    let value = next_json(&mut ws).await;
                    assert_ne!(value["type"], "error");
                    if value["type"] == "tabs" {
                        let sidebar = next_json(&mut ws).await;
                        assert_eq!(sidebar["type"], "session_sidebar");
                        assert_eq!(
                            sidebar["html"]
                                .as_str()
                                .unwrap()
                                .contains("data-session-workspace-group=\"runtime:/tmp/other\""),
                            op != "close_workspace"
                        );
                        break;
                    }
                }
            }
            let mut observed = Vec::new();
            while let Ok(op) = ops.try_recv() {
                observed.push(op);
            }
            assert!(observed.iter().any(|op| op == "interrupt"));
            assert!(!observed.iter().any(|op| op == "delete_session"));
        })
        .await
        .expect("workspace close must not use the forbidden delete_session operation");
    }

    #[tokio::test]
    async fn upload_files_refreshes_tree_and_notifies() {
        let upstream = spawn_fake_upstream().await;
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            ..test_state(None)
        })
        .await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let snapshot = next_json(&mut ws).await;
        assert_eq!(snapshot["type"], "snapshot");
        choose_test_workspace(&mut ws, snapshot["active"].as_u64().unwrap()).await;

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
            ..test_state(None)
        })
        .await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let snapshot = next_json(&mut ws).await;
        assert_eq!(snapshot["type"], "snapshot");
        choose_test_workspace(&mut ws, snapshot["active"].as_u64().unwrap()).await;

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
    async fn snapshot_shows_an_available_update() {
        let upstream = spawn_fake_upstream().await;
        let update = gewu_update::update_from_tags("0.1.4", "v0.1.5")
            .unwrap()
            .expect("newer release");
        let (_, updates) = watch::channel(Some(update));
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            updates,
            ..test_state(None)
        })
        .await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let snapshot = next_json(&mut ws).await;
        let sidebar = snapshot["session_sidebar"].as_str().unwrap();
        assert!(sidebar.contains("class=\"session-update\""), "{sidebar}");
        assert!(sidebar.contains("Update available: v0.1.5"), "{sidebar}");
    }

    #[tokio::test]
    async fn live_update_notice_is_pushed_to_the_sidebar() {
        let upstream = spawn_fake_upstream().await;
        let (update_tx, updates) = watch::channel(None);
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            updates,
            ..test_state(None)
        })
        .await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let snapshot = next_json(&mut ws).await;
        assert!(
            !snapshot["session_sidebar"]
                .as_str()
                .unwrap()
                .contains("session-update")
        );

        let update = gewu_update::update_from_tags("0.0.0-test", "v0.1.5")
            .unwrap()
            .expect("newer release");
        update_tx.send(Some(update)).unwrap();

        let sidebar = loop {
            let value = next_json(&mut ws).await;
            if value["type"] == "session_sidebar" {
                break value;
            }
        };
        let html = sidebar["html"].as_str().unwrap();
        assert!(html.contains("class=\"session-update\""), "{html}");
        assert!(html.contains("Update available: v0.1.5"), "{html}");
    }

    #[tokio::test]
    async fn browser_ask_streams_and_finishes() {
        let upstream = spawn_fake_upstream().await;
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            ..test_state(None)
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
        assert!(!sidebar.contains("<li>read</li>"));
        assert!(sidebar.contains("No workspace selected"));
        assert!(!sidebar.contains("/tmp/fake-project"));
        assert_eq!(snapshot["tabs"][0]["workspace"], "");

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
    async fn browser_shows_a_waiting_status_while_a_question_is_pending() {
        let upstream = spawn_fake_upstream().await;
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            ..test_state(None)
        })
        .await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let snapshot = next_json(&mut ws).await;
        assert_eq!(snapshot["type"], "snapshot");

        ws.send(WsMessage::text(
            r#"{"op":"ask","tab":1,"prompt":"question me"}"#,
        ))
        .await
        .unwrap();

        let mut waiting = false;
        let mut back_to_idle = false;
        let mut sidebar_waiting = false;
        let mut sidebar_settled = false;
        for _ in 0..80 {
            let value = next_json(&mut ws).await;
            let html = value["html"].as_str().unwrap_or("");
            match value["type"].as_str().unwrap_or("") {
                "status" => {
                    if html.contains("class=\"phase waiting\"") && html.contains("waiting…") {
                        assert!(!html.contains("thinking…"), "{html}");
                        waiting = true;
                    } else if waiting && html.contains(">idle</span>") {
                        back_to_idle = true;
                    }
                }
                "session_sidebar" => {
                    if html.contains("busy waiting") {
                        sidebar_waiting = true;
                    } else if sidebar_waiting {
                        sidebar_settled = true;
                    }
                }
                _ => {}
            }
            if back_to_idle && sidebar_settled {
                break;
            }
        }
        assert!(waiting, "a pending form should push the waiting phase");
        assert!(back_to_idle, "the finished turn should return to idle");
        assert!(
            sidebar_waiting,
            "the sidebar pulse should turn amber while waiting"
        );
        assert!(
            sidebar_settled,
            "the sidebar pulse should clear when the turn ends"
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
            ..test_state(None)
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
    async fn browser_can_switch_reasoning_effort_from_the_picker() {
        let (upstream, mut ops) = spawn_recorded_fake_upstream().await;
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            ..test_state(None)
        })
        .await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();

        let snapshot = next_json(&mut ws).await;
        assert_eq!(snapshot["type"], "snapshot");
        let status = snapshot["status"].as_str().unwrap();
        assert!(status.contains("effort-picker"), "{status}");
        assert!(
            status.contains("<option value=\"high\" selected>high</option>"),
            "{status}"
        );

        ws.send(WsMessage::text(
            r#"{"op":"switch_reasoning_effort","tab":1,"effort":"max"}"#,
        ))
        .await
        .unwrap();
        let mut switched = None;
        for _ in 0..20 {
            let value = next_json(&mut ws).await;
            if value["type"] == "status" {
                let html = value["html"].as_str().unwrap();
                if html.contains("<option value=\"max\" selected>max</option>") {
                    switched = Some(html.to_string());
                    break;
                }
            }
        }
        assert!(
            switched.is_some(),
            "the switched effort should be selected in a status update"
        );

        // The picker's "default" sends the provider-default sentinel, not a
        // reset to the configured default.
        while ops.try_recv().is_ok() {}
        ws.send(WsMessage::text(
            r#"{"op":"switch_reasoning_effort","tab":1,"effort":"provider-default"}"#,
        ))
        .await
        .unwrap();
        let mut provider_default = false;
        for _ in 0..20 {
            let value = next_json(&mut ws).await;
            if value["type"] == "status" {
                let html = value["html"].as_str().unwrap();
                if html.contains(
                    "<option value=\"provider-default\" title=\"provider default\" selected>default</option>",
                ) {
                    provider_default = true;
                    break;
                }
            }
        }
        assert!(
            provider_default,
            "the provider default should be selected in a status update"
        );
        let mut forwarded = false;
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(op) = ops.recv().await {
                if op == "switch_reasoning_effort" {
                    forwarded = true;
                    break;
                }
            }
        })
        .await;
        assert!(
            forwarded,
            "the effort switch should be forwarded to the upstream"
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
            ..test_state(None)
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

    /// `read_file` on an image must reach the card as an inline `<img>` once
    /// the card is expanded; the collapsed transcript only carries the badge.
    #[tokio::test]
    async fn tool_card_inlines_image_media() {
        let upstream = spawn_fake_upstream().await;
        let addr = spawn_gui(AppState {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: upstream.port(),
            },
            ..test_state(None)
        })
        .await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        assert_eq!(next_json(&mut ws).await["type"], "snapshot");

        ws.send(WsMessage::text(
            json!({"op": "ask", "tab": 1, "prompt": "show image"}).to_string(),
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
        assert!(transcript.contains("tool-badge\">image"), "{transcript}");
        assert!(!transcript.contains("<img"), "{transcript}");
        assert!(!transcript.contains("ZmFrZQ=="), "{transcript}");

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
            if value["type"] == "tool_card" {
                let html = value["html"].as_str().unwrap();
                assert!(
                    html.contains(
                        "<img class=\"tool-image\" src=\"data:image/png;base64,ZmFrZQ==\""
                    ),
                    "{html}"
                );
                assert!(html.contains("alt=\"shot.png\""), "{html}");
                return;
            }
        }
        panic!("expected a tool_card message");
    }

    /// The CSP must allow the data URLs tool cards use for images; Markdown
    /// still cannot reach it because `safe_dest` rewrites `data:` destinations.
    #[tokio::test]
    async fn index_allows_data_url_images() {
        let addr = spawn_gui(test_state(None)).await;
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            response.contains("img-src 'self' https: data:"),
            "{response}"
        );
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
            ..test_state(None)
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
            ..test_state(None)
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
            ..test_state(None)
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
