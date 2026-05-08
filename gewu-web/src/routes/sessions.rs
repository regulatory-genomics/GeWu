use crate::state::{AppState, SessionInfo};
use axum::{Json, extract::State};
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

#[derive(Debug, serde::Deserialize)]
pub struct CreateSessionRequest {
    pub agent_type: String,
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct SwitchAgentRequest {
    pub agent_type: String,
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
    let messages_json = attach_missing_workflow_trace_messages(&app, &id, messages_json).await?;
    let messages_json = attach_active_turn_message(&app, &id, messages_json).await;
    Ok(Json(MessagesResponse {
        messages: messages_json,
    }))
}

async fn attach_active_turn_message(
    app: &AppState,
    session_id: &str,
    mut messages: Vec<Value>,
) -> Vec<Value> {
    let Some(turn) = app.get_active_turn(session_id).await else {
        return messages;
    };
    if turn.reasoning.is_empty() && turn.content.is_empty() {
        return messages;
    }
    messages.push(json!({
        "role": "assistant",
        "content": turn.content,
        "tool_calls": null,
        "active_turn": true,
        "reasoning": turn.reasoning,
        "started_at": turn.started_at,
        "last_event_at": turn.last_event_at,
        "event_count": turn.event_count,
    }));
    messages
}

async fn attach_missing_workflow_trace_messages(
    app: &AppState,
    session_id: &str,
    messages: Vec<Value>,
) -> Result<Vec<Value>, AppError> {
    let missing_calls = missing_run_workflow_calls(&messages);
    if missing_calls.is_empty() {
        return Ok(messages);
    }

    let pool = &app.db_pool;

    let rows = sqlx::query(
        r#"
        SELECT
            tool.id AS tool_run_id,
            tool.name AS tool_name,
            tool.status AS tool_status,
            tool.started_at AS tool_started_at,
            tool.finished_at AS tool_finished_at,
            tool.error AS tool_error,
            workflow.id AS workflow_run_id,
            workflow.name AS workflow_name,
            workflow.status AS workflow_status,
            workflow.started_at AS workflow_started_at,
            workflow.finished_at AS workflow_finished_at,
            workflow.error AS workflow_error
        FROM execution_runs tool
        LEFT JOIN execution_runs workflow
            ON workflow.session_id = tool.session_id
           AND workflow.kind = 'workflow'
           AND workflow.started_at = tool.started_at
        WHERE tool.session_id = ?
          AND tool.kind = 'tool'
          AND tool.name = 'run_workflow'
        ORDER BY tool.started_at ASC, tool.id ASC
        "#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to load workflow trace runs: {e}")))?;

    let mut traces = rows
        .into_iter()
        .map(|row| WorkflowTraceMessage {
            tool_run_id: row.try_get("tool_run_id").unwrap_or_default(),
            tool_name: row
                .try_get("tool_name")
                .unwrap_or_else(|_| "run_workflow".to_string()),
            tool_status: row.try_get("tool_status").unwrap_or_default(),
            tool_started_at: row.try_get("tool_started_at").unwrap_or_default(),
            tool_finished_at: row.try_get("tool_finished_at").ok(),
            tool_error: row.try_get("tool_error").ok(),
            workflow_run_id: row.try_get("workflow_run_id").ok(),
            workflow_name: row.try_get("workflow_name").ok(),
            workflow_status: row.try_get("workflow_status").ok(),
            workflow_started_at: row.try_get("workflow_started_at").ok(),
            workflow_finished_at: row.try_get("workflow_finished_at").ok(),
            workflow_error: row.try_get("workflow_error").ok(),
        })
        .collect::<VecDeque<_>>();

    if traces.is_empty() {
        return Ok(messages);
    }

    let missing_call_ids: HashSet<String> = missing_calls.into_iter().collect();
    let mut enriched = Vec::with_capacity(messages.len() + missing_call_ids.len());
    for message in messages {
        let synthetic_tools =
            synthetic_workflow_tools_after_message(&message, &missing_call_ids, &mut traces);
        enriched.push(message);
        enriched.extend(synthetic_tools);
    }

    Ok(enriched)
}

fn missing_run_workflow_calls(messages: &[Value]) -> Vec<String> {
    let mut run_workflow_calls = Vec::new();
    let mut tool_results = HashSet::new();

    for message in messages {
        if message.get("role").and_then(Value::as_str) == Some("assistant") {
            for call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let name = call
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str);
                if name == Some("run_workflow") {
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        run_workflow_calls.push(id.to_string());
                    }
                }
            }
        } else if message.get("role").and_then(Value::as_str) == Some("tool") {
            if let Some(id) = message.get("tool_call_id").and_then(Value::as_str) {
                tool_results.insert(id.to_string());
            }
        }
    }

    run_workflow_calls
        .into_iter()
        .filter(|id| !tool_results.contains(id))
        .collect()
}

fn synthetic_workflow_tools_after_message(
    message: &Value,
    missing_call_ids: &HashSet<String>,
    traces: &mut VecDeque<WorkflowTraceMessage>,
) -> Vec<Value> {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }

    let mut synthetic = Vec::new();
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let call_id = call.get("id").and_then(Value::as_str);
        let name = call
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str);
        if name != Some("run_workflow") || !call_id.is_some_and(|id| missing_call_ids.contains(id))
        {
            continue;
        }

        let Some(trace) = traces.pop_front() else {
            break;
        };
        let Some(call_id) = call_id else {
            continue;
        };

        synthetic.push(json!({
            "role": "tool",
            "tool_call_id": call_id,
            "tool_name": trace.tool_name,
            "content": trace.to_output_json(),
        }));
    }

    synthetic
}

#[derive(Clone)]
struct WorkflowTraceMessage {
    tool_run_id: String,
    tool_name: String,
    tool_status: String,
    tool_started_at: i64,
    tool_finished_at: Option<i64>,
    tool_error: Option<String>,
    workflow_run_id: Option<String>,
    workflow_name: Option<String>,
    workflow_status: Option<String>,
    workflow_started_at: Option<i64>,
    workflow_finished_at: Option<i64>,
    workflow_error: Option<String>,
}

impl WorkflowTraceMessage {
    fn to_output_json(&self) -> String {
        let trace_run_id = self.workflow_run_id.as_ref().unwrap_or(&self.tool_run_id);
        serde_json::to_string_pretty(&json!({
            "status": self.workflow_status.as_ref().unwrap_or(&self.tool_status),
            "workflow_id": self.workflow_name,
            "tool_run_id": self.tool_run_id,
            "tool_status": self.tool_status,
            "tool_started_at": self.tool_started_at,
            "tool_finished_at": self.tool_finished_at,
            "tool_error": self.tool_error,
            "workflow_run_id": self.workflow_run_id,
            "workflow_started_at": self.workflow_started_at,
            "workflow_finished_at": self.workflow_finished_at,
            "workflow_error": self.workflow_error,
            "_trace": {
                "root_run_id": trace_run_id,
                "current_run_id": trace_run_id,
            }
        }))
        .unwrap_or_else(|_| "{}".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_run_workflow_calls_missing_tool_results() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_missing",
                    "type": "function",
                    "function": { "name": "run_workflow", "arguments": "{}" }
                }]
            }),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_present",
                    "type": "function",
                    "function": { "name": "run_workflow", "arguments": "{}" }
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call_present",
                "tool_name": "run_workflow",
                "content": "{}"
            }),
        ];

        assert_eq!(missing_run_workflow_calls(&messages), vec!["call_missing"]);
    }

    #[test]
    fn synthesizes_trace_tool_message_after_matching_assistant_call() {
        let message = json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_workflow",
                "type": "function",
                "function": { "name": "run_workflow", "arguments": "{}" }
            }]
        });
        let missing_call_ids = HashSet::from(["call_workflow".to_string()]);
        let trace = WorkflowTraceMessage {
            tool_run_id: "run_tool".to_string(),
            tool_name: "run_workflow".to_string(),
            tool_status: "succeeded".to_string(),
            tool_started_at: 1,
            tool_finished_at: Some(2),
            tool_error: None,
            workflow_run_id: Some("run_workflow_trace".to_string()),
            workflow_name: Some("daily_opencode_rollup".to_string()),
            workflow_status: Some("succeeded".to_string()),
            workflow_started_at: Some(1),
            workflow_finished_at: Some(2),
            workflow_error: None,
        };
        let mut traces = VecDeque::from([trace]);

        let synthetic =
            synthetic_workflow_tools_after_message(&message, &missing_call_ids, &mut traces);

        assert_eq!(synthetic.len(), 1);
        assert_eq!(synthetic[0]["role"], "tool");
        assert_eq!(synthetic[0]["tool_call_id"], "call_workflow");
        assert_eq!(synthetic[0]["tool_name"], "run_workflow");
        let content: Value =
            serde_json::from_str(synthetic[0]["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["_trace"]["root_run_id"], "run_workflow_trace");
        assert_eq!(content["workflow_id"], "daily_opencode_rollup");
    }

    #[test]
    fn agents_and_skills_responses_are_serializable() {
        let agents = AgentsResponse {
            agents: vec![AgentInfoResponse {
                name: "build".to_string(),
                description: "build agent".to_string(),
            }],
        };
        let skills = crate::routes::skills::SkillsResponse {
            skills: vec![crate::state::SkillInfo {
                name: "gene-locus-resolver".to_string(),
                description: "Resolve gene symbols".to_string(),
            }],
        };

        let agents_json = serde_json::to_value(agents).unwrap();
        let skills_json = serde_json::to_value(skills).unwrap();

        assert_eq!(agents_json["agents"][0]["name"], "build");
        assert_eq!(skills_json["skills"][0]["name"], "gene-locus-resolver");
    }
}

/// Probe whether the caller is authenticated. The middleware enforces auth
/// for protected routes; this endpoint is registered on the public group so
/// the frontend can detect open vs password mode and decide whether to show
/// the login UI. In open mode (`AuthMode::None`) it always returns valid;
/// in password mode it inspects the cookie.
pub async fn handle_validate(
    State(app): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Json<ValidateResponse> {
    let mode = app.auth_mode.read().await.clone();
    if matches!(mode, crate::auth::AuthMode::None) {
        return Json(ValidateResponse { valid: true });
    }
    let cookie = headers
        .get(axum::http::header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .and_then(crate::auth::extract_session_cookie)
        .map(str::to_string);
    let valid = match cookie {
        Some(c) => app.cookie_store.validate(&c).await,
        None => false,
    };
    Json(ValidateResponse { valid })
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
    let pool = &app.db_pool;

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
    let session = app.create_session(req.agent_type, req.project_id).await?;
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

pub async fn handle_cancel_session(
    State(app): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<MessageResponse>, AppError> {
    let cancelled = app.cancel_session(&id).await;
    if cancelled {
        Ok(Json(MessageResponse {
            message: "Session cancelled".to_string(),
        }))
    } else {
        Ok(Json(MessageResponse {
            message: "No active turn to cancel".to_string(),
        }))
    }
}

#[derive(Debug)]
pub enum AppError {
    Unauthorized,
    SessionNotFound,
    BadRequest(String),
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
            AppError::SessionNotFound => (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({ "error": "Session not found" })),
            )
                .into_response(),
            AppError::BadRequest(msg) => (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({ "error": msg })),
            )
                .into_response(),
            AppError::InternalError(msg) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": msg })),
            )
                .into_response(),
        }
    }
}
