use crate::state::AppState;
use axum::{
    extract::State,
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
    #[serde(default)]
    pub agent_type: Option<String>,
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
    Json(req): Json<ChatRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, AppError> {
    info!("handle_chat called for session: {}", req.session_id);

    // Auth is enforced by `auth_middleware::require_auth` on the protected
    // route group. The handler trusts the cookie validation done there.

    let fallback_type = req.agent_type.as_deref().unwrap_or("general");
    let (session_id, agent_handle) = app
        .ensure_agent(&req.session_id, fallback_type)
        .await
        .map_err(|e| {
            warn!("ensure_agent failed for {}: {}", req.session_id, e);
            AppError::SessionNotFound
        })?;

    info!("Agent found for session: {}, starting chat", session_id);

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let message = req.message.clone();
    app.start_active_turn(&session_id).await;

    let session_id_for_cleanup = session_id.clone();
    let app_for_cleanup = app.clone();
    let join_handle = tokio::spawn(async move {
        let mut agent = agent_handle.agent.lock().await;
        info!("Agent locked, calling ask() with message: {}", message);
        let stream = agent.ask(&message);
        futures::pin_mut!(stream);
        let mut event_count = 0usize;
        let mut client_connected = true;
        while let Some(result) = stream.next().await {
            event_count += 1;
            app.touch_active_turn(&session_id, event_count).await;
            debug!("Stream received event #{}: {:?}", event_count, result);
            let sse_event = match result {
                Ok(event) => match event {
                    gewu_core::types::AgentEvent::ReasoningDelta { delta } => {
                        app.append_active_turn_reasoning(&session_id, &delta).await;
                        SseEvent::reasoning_delta(delta).to_sse_event()
                    }
                    gewu_core::types::AgentEvent::ContentDelta { delta } => {
                        app.append_active_turn_content(&session_id, &delta).await;
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
            if client_connected
                && tx
                    .send(Ok::<Event, std::convert::Infallible>(sse_event))
                    .await
                    .is_err()
            {
                warn!("SSE channel closed; continuing agent execution without client stream");
                client_connected = false;
            }
        }
        app.finish_active_turn(&session_id).await;
        app.remove_abort_handles(&session_id).await;
        info!("Stream finished for session ({} events sent)", event_count);
    });

    app_for_cleanup
        .register_abort_handle(&session_id_for_cleanup, join_handle.abort_handle())
        .await;

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
