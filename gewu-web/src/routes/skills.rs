use crate::{
    routes::sessions::AppError,
    state::{AppState, SkillInfo},
};
use axum::{Json, extract::State};
use std::sync::Arc;

#[derive(Debug, serde::Serialize)]
pub struct SkillsResponse {
    pub skills: Vec<SkillInfo>,
}

pub async fn handle_list_skills(
    State(app): State<Arc<AppState>>,
) -> Result<Json<SkillsResponse>, AppError> {
    let skills = app.list_available_skills().await?;
    Ok(Json(SkillsResponse { skills }))
}
