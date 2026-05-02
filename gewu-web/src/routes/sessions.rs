use crate::state::{AppState, SessionInfo};
use axum::{
    extract::{Query, State},
    Json,
};
use std::sync::Arc;

#[derive(Debug, serde::Deserialize)]
pub struct CreateSessionRequest {
    pub agent_type: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct SwitchAgentRequest {
    pub agent_type: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct TokenQuery {
    pub token: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct SessionResponse {
    pub session: SessionInfo,
}

#[derive(Debug, serde::Serialize)]
pub struct SessionsListResponse {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, serde::Serialize)]
pub struct SessionChildInfo {
    pub id: String,
    pub agent_type: String,
    pub title: Option<String>,
    pub execution_status: Option<String>,
    pub result_status: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, serde::Serialize)]
pub struct SessionTreeNode {
    pub session: SessionInfo,
    pub children: Vec<SessionChildInfo>,
}

#[derive(Debug, serde::Serialize)]
pub struct SessionsTreeResponse {
    pub sessions: Vec<SessionTreeNode>,
}

#[derive(Debug, serde::Serialize)]
pub struct MessageResponse {
    pub message: String,
}

#[derive(Debug, serde::Serialize)]
pub struct ServerInfo {
    pub model: String,
    pub endpoint: String,
}

#[derive(Debug, serde::Serialize)]
pub struct AgentInfoResponse {
    pub name: String,
    pub description: String,
}

#[derive(Debug, serde::Serialize)]
pub struct AgentsResponse {
    pub agents: Vec<AgentInfoResponse>,
}

#[derive(Debug, serde::Serialize)]
pub struct ValidateResponse {
    pub valid: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct MessagesResponse {
    pub messages: Vec<serde_json::Value>,
}

pub async fn handle_get_messages(
    State(app): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<MessagesResponse>, AppError> {
    let messages = app.get_session_messages(&id).await?;
    let messages_json: Vec<serde_json::Value> = messages
        .into_iter()
        .map(|m| serde_json::to_value(m).unwrap_or(serde_json::Value::Null))
        .collect();
    Ok(Json(MessagesResponse {
        messages: messages_json,
    }))
}

pub async fn handle_validate(
    State(app): State<Arc<AppState>>,
    Query(query): Query<TokenQuery>,
) -> Result<Json<ValidateResponse>, AppError> {
    let token = query.token.as_ref().ok_or(AppError::Unauthorized)?;
    let auth = app.auth_token.read().await;
    if !auth.validate(token) {
        return Err(AppError::Unauthorized);
    }
    Ok(Json(ValidateResponse { valid: true }))
}

/// Returns the auth token without requiring authentication.
/// Safe for local dev servers; allows cross-origin frontends to auto-connect.
pub async fn handle_get_token(State(app): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let auth = app.auth_token.read().await;
    Json(serde_json::json!({ "token": auth.valid_token }))
}

pub async fn handle_server_info(
    State(app): State<Arc<AppState>>,
) -> Result<Json<ServerInfo>, AppError> {
    let config = app.get_config().await;
    Ok(Json(ServerInfo {
        model: config.model.unwrap_or_else(|| "unknown".to_string()),
        endpoint: config.endpoint.unwrap_or_else(|| "unknown".to_string()),
    }))
}

pub async fn handle_list_agents(
    State(app): State<Arc<AppState>>,
) -> Result<Json<AgentsResponse>, AppError> {
    let agents = app
        .list_available_agents()
        .await?
        .into_iter()
        .map(|(name, description)| AgentInfoResponse { name, description })
        .collect();
    Ok(Json(AgentsResponse { agents }))
}

pub async fn handle_list_sessions(
    State(app): State<Arc<AppState>>,
) -> Result<Json<SessionsListResponse>, AppError> {
    let sessions = app.list_sessions().await;
    Ok(Json(SessionsListResponse { sessions }))
}

pub async fn handle_sessions_tree(
    State(app): State<Arc<AppState>>,
) -> Result<Json<SessionsTreeResponse>, AppError> {
    let sessions = app.list_sessions().await;
    let pool_guard = app.db_pool.lock().await;
    let pool = pool_guard
        .as_ref()
        .ok_or_else(|| AppError::InternalError("Database is not initialized".to_string()))?;

    let mut nodes = Vec::new();
    for session in sessions {
        let rows = sqlx::query(
            r#"
            SELECT task_id, specialist_id, prompt, execution_status, result_status, child_session_id, started_at
            FROM tasks
            WHERE parent_session_id = ? AND child_session_id IS NOT NULL
            ORDER BY COALESCE(started_at, 0), task_id
            "#,
        )
        .bind(&session.id)
        .fetch_all(pool)
        .await
        .map_err(|err| AppError::InternalError(err.to_string()))?;

        let children = rows
            .into_iter()
            .filter_map(|row| {
                let id: Option<String> = sqlx::Row::try_get(&row, "child_session_id").ok();
                Some(SessionChildInfo {
                    id: id?,
                    agent_type: sqlx::Row::try_get(&row, "specialist_id")
                        .unwrap_or_else(|_| "specialist".to_string()),
                    title: sqlx::Row::try_get(&row, "prompt").ok(),
                    execution_status: sqlx::Row::try_get(&row, "execution_status").ok(),
                    result_status: sqlx::Row::try_get(&row, "result_status").ok(),
                    created_at: sqlx::Row::try_get(&row, "started_at")
                        .ok()
                        .flatten()
                        .unwrap_or(session.created_at),
                })
            })
            .collect();

        nodes.push(SessionTreeNode { session, children });
    }

    Ok(Json(SessionsTreeResponse { sessions: nodes }))
}

pub async fn handle_create_session(
    State(app): State<Arc<AppState>>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<Json<SessionResponse>, AppError> {
    let session = app.create_session(req.agent_type).await?;
    Ok(Json(SessionResponse { session }))
}

pub async fn handle_delete_session(
    State(app): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<MessageResponse>, AppError> {
    app.delete_session(&id).await?;
    Ok(Json(MessageResponse {
        message: "Session deleted".to_string(),
    }))
}

pub async fn handle_switch_agent(
    State(app): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SwitchAgentRequest>,
) -> Result<Json<SessionResponse>, AppError> {
    let session = app.switch_agent(&id, req.agent_type).await?;
    Ok(Json(SessionResponse { session }))
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
