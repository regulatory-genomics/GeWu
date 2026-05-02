use crate::state::AppState;
use axum::{
    extract::{Query, State},
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures::Stream;
use futures::StreamExt;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

#[derive(Debug, serde::Deserialize)]
pub struct ChatRequest {
    pub message: String,
    pub session_id: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct TokenQuery {
    pub token: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct SseEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: serde_json::Value,
}

impl SseEvent {
    pub fn reasoning_delta(delta: String) -> Self {
        Self {
            event_type: "reasoning_delta".to_string(),
            data: serde_json::json!({ "delta": delta }),
        }
    }

    pub fn content_delta(delta: String) -> Self {
        Self {
            event_type: "content_delta".to_string(),
            data: serde_json::json!({ "delta": delta }),
        }
    }

    pub fn tool_start(
        tool_call_id: String,
        tool: String,
        input: String,
        metadata: serde_json::Value,
    ) -> Self {
        Self {
            event_type: "tool_start".to_string(),
            data: serde_json::json!({ "tool_call_id": tool_call_id, "tool": tool, "input": input, "metadata": metadata }),
        }
    }

    pub fn tool_finish(tool_call_id: String, tool: String, output: String) -> Self {
        Self {
            event_type: "tool_finish".to_string(),
            data: serde_json::json!({ "tool_call_id": tool_call_id, "tool": tool, "output": output }),
        }
    }

    pub fn finish(response: String) -> Self {
        Self {
            event_type: "finish".to_string(),
            data: serde_json::json!({ "response": response }),
        }
    }

    pub fn error(error: String) -> Self {
        Self {
            event_type: "error".to_string(),
            data: serde_json::json!({ "error": error }),
        }
    }

    pub fn to_sse_event(&self) -> Event {
        Event::default()
            .event(self.event_type.clone())
            .data(serde_json::to_string(self).unwrap_or_default())
    }
}

pub async fn handle_chat(
    State(app): State<Arc<AppState>>,
    Query(query): Query<TokenQuery>,
    Json(req): Json<ChatRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, AppError> {
    info!("handle_chat called for session: {}", req.session_id);

    let auth = app.auth_token.read().await;
    let valid = query
        .token
        .as_ref()
        .map(|t| auth.validate(t))
        .unwrap_or(false);
    drop(auth);

    if !valid {
        warn!("Unauthorized chat attempt");
        return Err(AppError::Unauthorized);
    }

    let agent_handle = app.get_agent(&req.session_id).await;
    let agent_handle = agent_handle.ok_or(AppError::SessionNotFound)?;

    info!("Agent found for session: {}, starting chat", req.session_id);

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let message = req.message.clone();

    tokio::spawn(async move {
        let mut agent = agent_handle.agent.lock().await;
        info!("Agent locked, calling ask() with message: {}", message);
        let stream = agent.ask(&message);
        futures::pin_mut!(stream);
        let mut event_count = 0usize;
        while let Some(result) = stream.next().await {
            event_count += 1;
            debug!("Stream received event #{}: {:?}", event_count, result);
            let sse_event = match result {
                Ok(event) => match event {
                    gewu_core::types::AgentEvent::ReasoningDelta { delta } => {
                        SseEvent::reasoning_delta(delta).to_sse_event()
                    }
                    gewu_core::types::AgentEvent::ContentDelta { delta } => {
                        SseEvent::content_delta(delta).to_sse_event()
                    }
                    gewu_core::types::AgentEvent::ToolStart {
                        tool_call_id,
                        tool,
                        input,
                        metadata,
                    } => {
                        info!("tool_start: {} - {}", tool, input);
                        SseEvent::tool_start(tool_call_id, tool, input, metadata).to_sse_event()
                    }
                    gewu_core::types::AgentEvent::ToolFinish {
                        tool_call_id,
                        tool,
                        output,
                    } => {
                        info!("tool_finish: {} - {}", tool, output);
                        SseEvent::tool_finish(tool_call_id, tool, output).to_sse_event()
                    }
                    gewu_core::types::AgentEvent::Finish { response } => {
                        info!("finish: {}", response);
                        SseEvent::finish(response).to_sse_event()
                    }
                    gewu_core::types::AgentEvent::Error { error } => {
                        error!("agent error: {}", error);
                        SseEvent::error(error).to_sse_event()
                    }
                },
                Err(e) => {
                    error!("stream error: {}", e);
                    SseEvent::error(e.to_string()).to_sse_event()
                }
            };
            if tx
                .send(Ok::<Event, std::convert::Infallible>(sse_event))
                .await
                .is_err()
            {
                warn!("SSE channel closed, stopping stream");
                break;
            }
        }
        info!("Stream finished for session ({} events sent)", event_count);
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[derive(Debug)]
pub enum AppError {
    Unauthorized,
    SessionNotFound,
    InternalError(String),
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::InternalError(e.to_string())
    }
}

impl axum::response::IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        match self {
            AppError::Unauthorized => {
                (axum::http::StatusCode::UNAUTHORIZED, "Unauthorized").into_response()
            }
            AppError::SessionNotFound => {
                (axum::http::StatusCode::NOT_FOUND, "Session not found").into_response()
            }
            AppError::InternalError(msg) => {
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
            }
        }
    }
}
