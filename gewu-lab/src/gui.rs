use crate::{Lab, TaskStatus};
use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header, uri::Authority},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;

pub struct LabGui {
    listener: TcpListener,
    state: GuiState,
}

#[derive(Clone)]
struct GuiState {
    lab: Arc<Mutex<Lab>>,
    token: Option<Arc<str>>,
    chat: Option<Chat>,
}

#[derive(Clone)]
struct Chat {
    provider: Arc<dyn gewu_core::traits::LlmProvider>,
    permit: Arc<tokio::sync::Semaphore>,
}

impl LabGui {
    pub async fn bind(lab: Arc<Mutex<Lab>>, port: u16) -> io::Result<Self> {
        Self::bind_to(lab, (Ipv4Addr::LOCALHOST, port).into(), None).await
    }

    pub async fn bind_to(
        lab: Arc<Mutex<Lab>>,
        address: SocketAddr,
        token: Option<String>,
    ) -> io::Result<Self> {
        if token
            .as_ref()
            .is_some_and(|token| token.trim().is_empty() || token.len() > 1024)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "token must be nonempty and at most 1024 bytes",
            ));
        }
        if !address.ip().is_loopback() && token.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote access requires --token or GEWU_LAB_TOKEN",
            ));
        }
        Ok(Self {
            listener: TcpListener::bind(address).await?,
            state: GuiState {
                lab,
                token: token.map(Arc::from),
                chat: None,
            },
        })
    }

    pub fn url(&self) -> String {
        format!(
            "http://{}/",
            self.listener.local_addr().expect("bound TCP listener")
        )
    }

    pub fn with_chat(
        mut self,
        provider: Arc<dyn gewu_core::traits::LlmProvider>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            self.state.token.is_some(),
            "Lab chat requires --token or GEWU_LAB_TOKEN"
        );
        self.state.chat = Some(Chat {
            provider,
            permit: Arc::new(tokio::sync::Semaphore::new(1)),
        });
        Ok(self)
    }

    pub async fn serve(self) -> io::Result<()> {
        axum::serve(self.listener, router(self.state)).await
    }
}

fn router(state: GuiState) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../assets/board.html")) }),
        )
        .route(
            "/board.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../assets/board.js"),
                )
            }),
        )
        .route(
            "/board.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../assets/board.css"),
                )
            }),
        )
        .route("/ws", get(upgrade))
        .with_state(state)
}

async fn upgrade(
    State(state): State<GuiState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !origin_allowed(&headers, state.token.is_none()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    ws.max_message_size(8192)
        .max_frame_size(8192)
        .on_upgrade(move |socket| stream_board(socket, state))
}

fn origin_allowed(headers: &HeaderMap, local_only: bool) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .and_then(|host| host.parse::<Authority>().ok())
    else {
        return false;
    };
    if local_only
        && !host
            .host()
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
        && !host.host().eq_ignore_ascii_case("localhost")
    {
        return false;
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin = origin.to_str().ok().and_then(|origin| {
            origin
                .strip_prefix("http://")
                .or_else(|| origin.strip_prefix("https://"))
        });
        return origin.is_some_and(|origin| origin.eq_ignore_ascii_case(host.as_str()));
    }
    true
}

#[derive(serde::Deserialize)]
struct Hello {
    token: Option<String>,
}

async fn stream_board(mut socket: WebSocket, state: GuiState) {
    if let Some(expected) = &state.token {
        let authorized = match tokio::time::timeout(Duration::from_secs(5), socket.recv()).await {
            Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<Hello>(&text)
                .ok()
                .and_then(|hello| hello.token)
                .is_some_and(|token| bool::from(token.as_bytes().ct_eq(expected.as_bytes()))),
            _ => false,
        };
        if !authorized {
            let _ = send(&mut socket, r#"{"type":"unauthorized"}"#).await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    }

    if state.chat.is_some() && !send(&mut socket, r#"{"type":"chat_ready"}"#).await {
        return;
    }
    let mut turn: Option<tokio::task::JoinSet<serde_json::Value>> = None;
    let mut refresh = tokio::time::interval(Duration::from_secs(3));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last = String::new();
    loop {
        tokio::select! {
            _ = refresh.tick() => {
                let Ok(projects) = snapshot(&state.lab) else { break };
                let Ok(data) = serde_json::to_string(&projects) else { break };
                if data != last {
                    if !send(&mut socket, &data).await { break; }
                    last = data;
                }
            }
            message = socket.recv() => {
                match message {
                    None | Some(Err(_) | Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Text(text))) if state.chat.is_some() => {
                        match start_chat(&state, &text, turn.is_some()) {
                            Ok(job) => turn = Some(job),
                            Err(error) => {
                                if !send(&mut socket, &chat_response("chat_error", error).to_string()).await { break; }
                            }
                        }
                    }
                    _ => {}
                }
            }
            reply = async { turn.as_mut().unwrap().join_next().await }, if turn.is_some() => {
                turn = None;
                let reply = reply.and_then(Result::ok).unwrap_or_else(|| chat_response("chat_error", "Lab could not answer."));
                if !send(&mut socket, &reply.to_string()).await { break; }
            }
        }
    }
}

fn chat_response(kind: &str, text: &str) -> serde_json::Value {
    serde_json::json!({"type": kind, "text": text})
}

#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatRequest {
    ChatSend { text: String },
}

fn start_chat(
    state: &GuiState,
    frame: &str,
    busy: bool,
) -> Result<tokio::task::JoinSet<serde_json::Value>, &'static str> {
    let ChatRequest::ChatSend { text } =
        serde_json::from_str(frame).map_err(|_| "Invalid chat request.")?;
    let text = text.trim().to_owned();
    if text.is_empty() || text.len() > 4096 {
        return Err("Enter a message of up to 4096 bytes.");
    }
    if busy {
        return Err("Lab chat is busy.");
    }
    let chat = state.chat.as_ref().ok_or("Lab chat is disabled.")?;
    let permit = chat
        .permit
        .clone()
        .try_acquire_owned()
        .map_err(|_| "Lab chat is busy.")?;
    let provider = chat.provider.clone();
    let lab = state.lab.clone();
    let mut job = tokio::task::JoinSet::new();
    job.spawn(async move {
        let _permit = permit;
        let project_provider = provider.clone();
        let task_provider = provider.clone();
        let dispatch_provider = provider.clone();
        let mut coordinator = crate::LabCoordinator::new(lab)
            .with_sync_projects(Arc::new(move |_| {
                Ok(gewu_core::agent::Agent::builder().llm(project_provider.clone()))
            }))
            .with_sync_tasks(Arc::new(move |_| {
                Ok(gewu_core::agent::Agent::builder().llm(task_provider.clone()))
            }))
            .with_task_dispatch(Arc::new(move |_task| {
                Ok(crate::TaskLoop::new(
                    gewu_core::agent::Agent::builder()
                        .llm(dispatch_provider.clone())
                        .build()?,
                ))
            }));
        let builder = gewu_core::agent::Agent::builder().llm(provider).max_turns(Some(12)).system_prompt("You are the Lab coordinator. Help the human manage projects using the Lab tools. ask_project_agent waits for the project agent's response and returns it to you. Report only confirmed changes.");
        match tokio::time::timeout(Duration::from_secs(120), coordinator.handle_human_message(builder, &text)).await {
            Ok(Ok(reply)) => chat_response("chat_reply", &reply),
            Ok(Err(_)) => chat_response("chat_error", "Lab could not answer. Project changes may already have been applied."),
            Err(_) => chat_response("chat_error", "Lab timed out. Project changes may already have been applied."),
        }
    });
    Ok(job)
}

async fn send(socket: &mut WebSocket, data: &str) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(5),
            socket.send(Message::Text(data.into()))
        )
        .await,
        Ok(Ok(()))
    )
}

#[derive(Serialize)]
struct ProjectView {
    id: String,
    complete: bool,
    tasks: Vec<TaskView>,
}

#[derive(Serialize)]
struct TaskView {
    id: String,
    name: String,
    status: &'static str,
    deliverable: String,
    dependencies: Vec<String>,
}

fn snapshot(lab: &Mutex<Lab>) -> Result<Vec<ProjectView>, StatusCode> {
    let projects = {
        let lab = lab.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        lab.projects()
            .map(|(id, project)| ProjectView {
                id: id.into(),
                complete: project.is_complete(),
                tasks: project
                    .task_board()
                    .map(|(task, dependencies)| TaskView {
                        id: task.id.clone(),
                        name: task.name.clone(),
                        status: match task.status {
                            TaskStatus::Pending => "Pending",
                            TaskStatus::InProgress => "In progress",
                            TaskStatus::AwaitingReview => "Awaiting review",
                            TaskStatus::Completed => "Completed",
                            TaskStatus::Blocked => "Blocked",
                            TaskStatus::Cancelled => "Cancelled",
                        },
                        deliverable: task.deliverable.clone(),
                        dependencies: dependencies.iter().cloned().collect(),
                    })
                    .collect(),
            })
            .collect()
    };
    Ok(projects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Project;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{connect_async, tungstenite::Message as Frame};

    #[tokio::test]
    async fn websocket_authenticates_and_reconnects_to_current_board() {
        let lab = Arc::new(Mutex::new(Lab::new()));
        assert!(
            LabGui::bind_to(lab.clone(), "0.0.0.0:0".parse().unwrap(), None)
                .await
                .is_err()
        );
        for token in [None, Some("test-only".to_owned())] {
            let gui = LabGui::bind_to(lab.clone(), "127.0.0.1:0".parse().unwrap(), token.clone())
                .await
                .unwrap();
            let url = format!("ws://{}/ws", gui.listener.local_addr().unwrap());
            let server = tokio::spawn(gui.serve());
            if token.is_some() {
                let (mut ws, _) = connect_async(&url).await.unwrap();
                ws.send(Frame::Text(r#"{"token":"wrong"}"#.into()))
                    .await
                    .unwrap();
                assert_eq!(
                    ws.next().await.unwrap().unwrap().into_text().unwrap(),
                    r#"{"type":"unauthorized"}"#
                );
            }
            for index in 0..2 {
                let id = format!("project-{}-{index}", token.is_some());
                lab.lock()
                    .unwrap()
                    .add_project(&id, Project::new())
                    .unwrap();
                let (mut ws, _) = connect_async(&url).await.unwrap();
                if token.is_some() {
                    ws.send(Frame::Text(r#"{"token":"test-only"}"#.into()))
                        .await
                        .unwrap();
                }
                let data = tokio::time::timeout(Duration::from_secs(2), ws.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .into_text()
                    .unwrap();
                assert!(data.contains(&id));
            }
            server.abort();
        }
    }

    #[test]
    fn browser_origin_must_match_and_anonymous_access_must_be_local() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "localhost:8440".parse().unwrap());
        headers.insert(header::ORIGIN, "https://elsewhere.test".parse().unwrap());
        assert!(!origin_allowed(&headers, true));
        headers.insert(header::ORIGIN, "http://localhost:8440".parse().unwrap());
        assert!(origin_allowed(&headers, true));
        headers.insert(header::HOST, "lab.example.test".parse().unwrap());
        headers.insert(header::ORIGIN, "https://lab.example.test".parse().unwrap());
        assert!(!origin_allowed(&headers, true));
        assert!(origin_allowed(&headers, false));
    }
}
