use crate::routes::sessions::{AppError, SessionChildInfo};
use crate::state::{AppState, ProjectInfo, SessionInfo};
use axum::{extract::State, Json};
use sqlx::Row;
use std::sync::Arc;

#[derive(Debug, serde::Deserialize)]
pub struct CreateProjectRequest {
    pub name: String,
    pub workdir: String,
}

#[derive(Debug, serde::Serialize)]
pub struct ProjectResponse {
    pub project: ProjectInfo,
}

#[derive(Debug, serde::Serialize)]
pub struct ProjectsListResponse {
    pub projects: Vec<ProjectInfo>,
}

#[derive(Debug, serde::Serialize)]
pub struct ProjectSessionNode {
    pub session: SessionInfo,
    pub children: Vec<SessionChildInfo>,
}

#[derive(Debug, serde::Serialize)]
pub struct ProjectTreeNode {
    pub project_id: String,
    pub name: String,
    pub workdir: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub sessions: Vec<ProjectSessionNode>,
}

#[derive(Debug, serde::Serialize)]
pub struct ProjectsTreeResponse {
    pub projects: Vec<ProjectTreeNode>,
}

#[derive(Debug, serde::Deserialize)]
pub struct DirectoryQuery {
    pub path: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct DirectoryBrowseResponse {
    pub directories: Vec<crate::state::DirectoryEntry>,
}

pub async fn handle_list_projects(
    State(app): State<Arc<AppState>>,
) -> Result<Json<ProjectsListResponse>, AppError> {
    let projects = app.list_projects().await?;
    Ok(Json(ProjectsListResponse { projects }))
}

pub async fn handle_create_project(
    State(app): State<Arc<AppState>>,
    Json(req): Json<CreateProjectRequest>,
) -> Result<Json<ProjectResponse>, AppError> {
    let project = app
        .create_project(req.name, req.workdir)
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?;
    Ok(Json(ProjectResponse { project }))
}

pub async fn handle_browse_directories(
    State(app): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<DirectoryQuery>,
) -> Result<Json<DirectoryBrowseResponse>, AppError> {
    let directories = app
        .browse_project_directories(query.path)
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?;
    Ok(Json(DirectoryBrowseResponse { directories }))
}

pub async fn handle_projects_tree(
    State(app): State<Arc<AppState>>,
) -> Result<Json<ProjectsTreeResponse>, AppError> {
    app.infer_legacy_session_projects().await?;
    let projects = app.project_store.list_all().await?;
    let sessions = app.list_sessions().await;

    let pool = &app.db_pool;

    let mut nodes = Vec::with_capacity(projects.len());
    for project in projects {
        let project_sessions = sessions
            .iter()
            .filter(|session| session.project_id.as_deref() == Some(project.project_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let mut session_nodes = Vec::with_capacity(project_sessions.len());
        for session in project_sessions {
            let children = load_session_children(pool, &session).await?;
            session_nodes.push(ProjectSessionNode { session, children });
        }

        nodes.push(ProjectTreeNode {
            project_id: project.project_id,
            name: project.name,
            workdir: project.workdir,
            created_at: project.created_at,
            updated_at: project.updated_at,
            sessions: session_nodes,
        });
    }

    Ok(Json(ProjectsTreeResponse { projects: nodes }))
}

async fn load_session_children(
    pool: &sqlx::SqlitePool,
    session: &SessionInfo,
) -> Result<Vec<SessionChildInfo>, AppError> {
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

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let id: Option<String> = Row::try_get(&row, "child_session_id").ok();
            Some(SessionChildInfo {
                id: id?,
                agent_type: Row::try_get(&row, "specialist_id")
                    .unwrap_or_else(|_| "specialist".to_string()),
                title: Row::try_get(&row, "prompt").ok(),
                execution_status: Row::try_get(&row, "execution_status").ok(),
                result_status: Row::try_get(&row, "result_status").ok(),
                created_at: Row::try_get(&row, "started_at")
                    .ok()
                    .flatten()
                    .unwrap_or(session.created_at),
            })
        })
        .collect())
}
