//! Type definitions for the Experience system.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Experience {
    pub id: String,
    pub task_description: String,
    pub task_type: String,
    pub problem_summary: String,
    pub solution_summary: String,
    pub tools_used: Vec<String>,
    pub iteration_count: i32,
    pub success: bool,
    pub embedding_id: String,
    #[serde(with = "chrono::serde::ts_seconds")]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub quality_score: f32,
    #[serde(default)]
    pub recall_count: i32,
}

impl Experience {
    pub fn compute_quality_score(solution: &str) -> f32 {
        let mut score = 0.0;

        let char_count = solution.len() as f32;
        score += (char_count / 300.0).min(2.0);

        if solution.contains("```") || solution.contains("`") {
            score += 2.0;
        }

        if solution.contains("# ") || solution.contains("\n- ")
            || solution.contains("\n1. ") || solution.contains("| ")
        {
            score += 1.0;
        }

        if solution.len() > 200 {
            score += 2.0;
        }

        score.max(0.0).min(8.0)
    }

    pub fn new(
        task_description: String,
        task_type: String,
        problem_summary: String,
        solution_summary: String,
        tools_used: Vec<String>,
        iteration_count: i32,
        success: bool,
        embedding_id: String,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            task_description,
            task_type,
            problem_summary,
            solution_summary,
            tools_used,
            iteration_count,
            success,
            embedding_id,
            created_at: chrono::Utc::now(),
            quality_score: 0.0,
            recall_count: 0,
        }
    }

    pub fn search_text(&self) -> String {
        format!(
            "Task: {} ({})\nProblem: {}\nSolution: {}\nTools used: {}",
            self.task_description,
            self.task_type,
            self.problem_summary,
            self.solution_summary,
            self.tools_used.join(", ")
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StoreResult {
    Created { id: String },
    Updated { id: String, action: String },
    Merged { id: String },
    Skipped { reason: String },
}

impl std::fmt::Display for StoreResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreResult::Created { id } => write!(f, "Created new experience: {}", id),
            StoreResult::Updated { id, action } => write!(f, "Updated experience {} ({})", id, action),
            StoreResult::Merged { id } => write!(f, "Merged into experience: {}", id),
            StoreResult::Skipped { reason } => write!(f, "Skipped: {}", reason),
        }
    }
}

impl StoreResult {
    pub fn id(&self) -> &str {
        match self {
            StoreResult::Created { id } => id,
            StoreResult::Updated { id, .. } => id,
            StoreResult::Merged { id } => id,
            StoreResult::Skipped { .. } => "",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExperienceQuery {
    pub query_text: String,
    #[serde(default)]
    pub task_type_filter: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default = "default_threshold")]
    pub score_threshold: f32,
}

fn default_limit() -> usize {
    5
}

fn default_threshold() -> f32 {
    0.7
}

impl Default for ExperienceQuery {
    fn default() -> Self {
        Self {
            query_text: String::new(),
            task_type_filter: None,
            limit: 5,
            score_threshold: 0.7,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecalledExperience {
    pub experience: Experience,
    pub similarity_score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    BugFix,
    FeatureImpl,
    Refactor,
    Research,
    Docs,
    Test,
    Other,
}

impl TaskType {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskType::BugFix => "bug_fix",
            TaskType::FeatureImpl => "feature_impl",
            TaskType::Refactor => "refactor",
            TaskType::Research => "research",
            TaskType::Docs => "docs",
            TaskType::Test => "test",
            TaskType::Other => "other",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "bug_fix" | "bugfix" | "bug" => TaskType::BugFix,
            "feature_impl" | "featureimpl" | "feature" => TaskType::FeatureImpl,
            "refactor" | "refactoring" => TaskType::Refactor,
            "research" | "investigation" => TaskType::Research,
            "docs" | "documentation" => TaskType::Docs,
            "test" | "testing" => TaskType::Test,
            _ => TaskType::Other,
        }
    }
}

impl std::fmt::Display for TaskType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_type_conversion() {
        assert_eq!(TaskType::from_str("bug_fix"), TaskType::BugFix);
        assert_eq!(TaskType::from_str("feature_impl"), TaskType::FeatureImpl);
        assert_eq!(TaskType::from_str("refactor"), TaskType::Refactor);
        assert_eq!(TaskType::from_str("unknown"), TaskType::Other);
    }

    #[test]
    fn test_experience_new() {
        let exp = Experience::new(
            "Fix memory leak".to_string(),
            "bug_fix".to_string(),
            "Memory grew unbounded".to_string(),
            "Added connection pool limits".to_string(),
            vec!["bash".to_string(), "grep".to_string()],
            2,
            true,
            "embedding_123".to_string(),
        );

        assert!(!exp.id.is_empty());
        assert_eq!(exp.task_type, "bug_fix");
        assert_eq!(exp.iteration_count, 2);
        assert!(exp.success);
    }

    #[test]
    fn test_experience_search_text() {
        let exp = Experience::new(
            "Fix memory leak".to_string(),
            "bug_fix".to_string(),
            "Memory grew unbounded".to_string(),
            "Added connection pool limits".to_string(),
            vec!["bash".to_string(), "grep".to_string()],
            2,
            true,
            "embedding_123".to_string(),
        );

        let search_text = exp.search_text();
        assert!(search_text.contains("Fix memory leak"));
        assert!(search_text.contains("bug_fix"));
        assert!(search_text.contains("bash"));
    }

    #[test]
    fn test_experience_query_default() {
        let query = ExperienceQuery::default();
        assert_eq!(query.limit, 5);
        assert_eq!(query.score_threshold, 0.7);
        assert!(query.task_type_filter.is_none());
    }
}
