//! Tools for storing and recalling experiences.

use crate::store::ExperienceStore;
use crate::types::{Experience, ExperienceQuery};
use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::sync::Arc;
use tracing::error;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct StoreExperienceArgs {
    pub task_description: String,
    pub task_type: String,
    pub problem_summary: String,
    pub solution_summary: String,
    #[serde(default)]
    pub tools_used: Vec<String>,
    #[serde(default = "default_iteration_count")]
    pub iteration_count: i32,
    #[serde(default = "default_success")]
    pub success: bool,
}

fn default_iteration_count() -> i32 {
    1
}

fn default_success() -> bool {
    true
}

pub struct ExperienceStoreTool {
    store: Arc<ExperienceStore>,
}

impl ExperienceStoreTool {
    pub fn new(store: Arc<ExperienceStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for ExperienceStoreTool {
    fn name(&self) -> &str {
        "experience_store"
    }

    fn description(&self) -> &str {
        r#"Store a successful (or failed) experience for future recall.

Use this tool after completing a task to save the experience for future reference.
When similar tasks come up, the experience_recall tool can retrieve this knowledge.

Arguments:
- task_description: What was the original task or user request?
- task_type: Type of task (bug_fix, feature_impl, refactor, research, docs, test, other)
- problem_summary: What was the problem you solved?
- solution_summary: How did you solve it? What approach worked?
- tools_used: List of tools you used (will be auto-populated if not provided)
- iteration_count: How many attempts were needed? (default: 1)
- success: Was the task completed successfully? (default: true)"#
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(StoreExperienceArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: StoreExperienceArgs = serde_json::from_value(args)?;

        let embedding_id = format!("exp_{}", uuid::Uuid::new_v4());

        let experience = Experience::new(
            input.task_description,
            input.task_type,
            input.problem_summary,
            input.solution_summary,
            input.tools_used,
            input.iteration_count,
            input.success,
            embedding_id,
        );

        match self.store.store(&experience).await {
            Ok(()) => Ok(ToolResult::new(
                "experience_store",
                format!(
                    "Experience stored successfully.\nID: {}\nType: {}\nTask: {}",
                    experience.id,
                    experience.task_type,
                    experience.task_description
                ),
            )),
            Err(e) => {
                error!("Failed to store experience: {}", e);
                Err(e.into())
            }
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RecallExperienceArgs {
    pub query: String,
    #[serde(default)]
    pub task_type: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    5
}

pub struct ExperienceRecallTool {
    store: Arc<ExperienceStore>,
}

impl ExperienceRecallTool {
    pub fn new(store: Arc<ExperienceStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for ExperienceRecallTool {
    fn name(&self) -> &str {
        "experience_recall"
    }

    fn description(&self) -> &str {
        r#"Recall similar experiences from past problem-solving sessions.

Use this tool when starting a new task to see if you've solved something similar before.
The query should describe your current task or problem.

Arguments:
- query: Natural language query describing your current task or problem
- task_type: Optional filter by task type (bug_fix, feature_impl, refactor, research, docs, test)
- limit: Maximum number of experiences to return (default: 5)"#
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(RecallExperienceArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: RecallExperienceArgs = serde_json::from_value(args)?;

        let query = ExperienceQuery {
            query_text: input.query,
            task_type_filter: input.task_type,
            limit: input.limit,
            score_threshold: 0.7,
        };

        match self.store.recall(&query).await {
            Ok(results) => {
                if results.is_empty() {
                    return Ok(ToolResult::new(
                        "experience_recall",
                        "No similar experiences found. Consider this a novel problem.".to_string(),
                    ));
                }

                let mut output = String::from("Similar experiences found:\n\n");
                for (i, recalled) in results.iter().enumerate() {
                    let exp = &recalled.experience;
                    output.push_str(&format!(
                        "{}. [{}] {} (score: {:.2})\n   Problem: {}\n   Solution: {}\n   Tools: {}\n   Attempts: {}\n\n",
                        i + 1,
                        exp.task_type,
                        exp.task_description,
                        recalled.similarity_score,
                        exp.problem_summary,
                        exp.solution_summary,
                        exp.tools_used.join(", "),
                        exp.iteration_count
                    ));
                }

                Ok(ToolResult::new("experience_recall", output))
            }
            Err(e) => {
                error!("Failed to recall experiences: {}", e);
                Err(e.into())
            }
        }
    }
}
