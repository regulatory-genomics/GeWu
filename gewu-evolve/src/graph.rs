//! Evolution graph for iterative self-improvement with multi-agent orchestration.

use crate::critique::{CritiqueResult, SelfCritique};
use crate::executor::Executor;
use crate::experience_manager::ExperienceManager;
use crate::planner::Planner;
use crate::reviewer::{ReviewResult, Reviewer};
use crate::Config;
use anyhow::Result;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::Agent;
use gewu_graph::state::State;
use gewu_tools::{GlobTool, GrepTool, ListTool, ReadFileTool, WebFetchTool, WebSearchTool};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub step_id: i32,
    pub description: String,
    pub tool: Option<String>,
    pub parameters: Option<serde_json::Value>,
    pub depends_on: Vec<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub steps: Vec<PlanStep>,
    pub tools_needed: Vec<String>,
    pub reasoning: String,
    pub based_on_experience: Option<String>,
}

impl Plan {
    pub fn new(id: String, reasoning: String) -> Self {
        Self {
            id,
            steps: Vec::new(),
            tools_needed: Vec::new(),
            reasoning,
            based_on_experience: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionState {
    pub task: String,
    pub original_task: String,
    pub attempts: i32,
    pub plan_attempts: i32,
    pub max_attempts: i32,
    pub max_plan_attempts: i32,
    pub errors: Vec<String>,
    pub critique_history: Vec<CritiqueResult>,
    pub review_history: Vec<String>,
    pub current_plan: Option<String>,
    pub current_plan_id: Option<String>,
    pub completed: bool,
    pub aborted: bool,
    pub outcome: String,
    pub execution_output: String,
}

impl Default for EvolutionState {
    fn default() -> Self {
        Self {
            task: String::new(),
            original_task: String::new(),
            attempts: 0,
            plan_attempts: 0,
            max_attempts: 5,
            max_plan_attempts: 3,
            errors: Vec::new(),
            critique_history: Vec::new(),
            review_history: Vec::new(),
            current_plan: None,
            current_plan_id: None,
            completed: false,
            aborted: false,
            outcome: String::new(),
            execution_output: String::new(),
        }
    }
}

impl EvolutionState {
    pub fn new(task: String, max_attempts: i32) -> Self {
        Self {
            task: task.clone(),
            original_task: task,
            max_attempts,
            ..Default::default()
        }
    }

    pub fn with_config(task: String, config: &Config) -> Self {
        Self::new(task, config.max_attempts)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartialEvolutionState {
    pub task: Option<String>,
    pub original_task: Option<String>,
    pub attempts: Option<i32>,
    pub plan_attempts: Option<i32>,
    pub max_attempts: Option<i32>,
    pub max_plan_attempts: Option<i32>,
    pub errors: Option<Vec<String>>,
    pub critique_history: Option<Vec<CritiqueResult>>,
    pub review_history: Option<Vec<String>>,
    pub current_plan: Option<String>,
    pub current_plan_id: Option<String>,
    pub completed: Option<bool>,
    pub aborted: Option<bool>,
    pub outcome: Option<String>,
    pub execution_output: Option<String>,
}

impl Default for PartialEvolutionState {
    fn default() -> Self {
        Self {
            task: None,
            original_task: None,
            attempts: None,
            plan_attempts: None,
            max_attempts: None,
            max_plan_attempts: None,
            errors: None,
            critique_history: None,
            review_history: None,
            current_plan: None,
            current_plan_id: None,
            completed: None,
            aborted: None,
            outcome: None,
            execution_output: None,
        }
    }
}

impl State for EvolutionState {
    type Partial = PartialEvolutionState;

    fn merge(&self, update: Self::Partial) -> Self {
        Self {
            task: update.task.unwrap_or_else(|| self.task.clone()),
            original_task: update.original_task.unwrap_or_else(|| self.original_task.clone()),
            attempts: update.attempts.unwrap_or(self.attempts),
            plan_attempts: update.plan_attempts.unwrap_or(self.plan_attempts),
            max_attempts: update.max_attempts.unwrap_or(self.max_attempts),
            max_plan_attempts: update.max_plan_attempts.unwrap_or(self.max_plan_attempts),
            errors: update.errors.unwrap_or_else(|| self.errors.clone()),
            critique_history: update
                .critique_history
                .unwrap_or_else(|| self.critique_history.clone()),
            review_history: update
                .review_history
                .unwrap_or_else(|| self.review_history.clone()),
            current_plan: update.current_plan.or(self.current_plan.clone()),
            current_plan_id: update.current_plan_id.or(self.current_plan_id.clone()),
            completed: update.completed.unwrap_or(self.completed),
            aborted: update.aborted.unwrap_or(self.aborted),
            outcome: update.outcome.unwrap_or_else(|| self.outcome.clone()),
            execution_output: update.execution_output.unwrap_or_else(|| self.execution_output.clone()),
        }
    }
}

pub struct EvolutionGraph {
    config: Config,
}

impl EvolutionGraph {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub async fn run_multi_agent(
        &self,
        task: &str,
        llm: Arc<dyn LlmProvider>,
        _tools: Arc<dyn gewu_core::traits::ToolRegistry>,
        tools_list: Arc<Vec<Arc<dyn Tool>>>,
        experience_store: Option<Arc<gewu_experience::ExperienceStore>>,
    ) -> Result<EvolutionState> {
        let mut state = EvolutionState::with_config(task.to_string(), &self.config);

        let read_only_tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(ReadFileTool {
                working_directory: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            }),
            Arc::new(GlobTool {
                working_directory: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            }),
            Arc::new(GrepTool {
                working_directory: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            }),
            Arc::new(ListTool {
                working_directory: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            }),
            Arc::new(WebSearchTool),
            Arc::new(WebFetchTool),
        ];

        let planner_agent = Agent::builder()
            .llm(llm.clone())
            .with_tools(read_only_tools.clone())
            .system_prompt("You are a planner agent creating execution plans. When planning, you may use available tools to read files, search code, and gather information. However, you should NOT execute bash commands or modify files during planning.")
            .build()?;

        let reviewer_agent = Agent::builder()
            .llm(llm.clone())
            .with_tools(read_only_tools.clone())
            .system_prompt("You are a reviewer agent evaluating execution plans. When reviewing, you may use available tools to read files, search code, and gather information. However, you should NOT execute bash commands or modify files.")
            .build()?;

        let mut planner = Planner::new(planner_agent);
        let mut reviewer = Reviewer::new(reviewer_agent);

        let agent = Agent::builder()
            .llm(llm.clone())
            .with_tools((*tools_list).clone())
            .system_prompt("You are an execution agent. Execute the given plan using available tools. When done, provide a summary.")
            .build()?;

        let mut executor = Executor::new(agent);
        let experience_manager = ExperienceManager::new(experience_store);

        tracing::info!("Starting multi-agent evolution for task: {}", task);

        while state.attempts < state.max_attempts && !state.completed && !state.aborted {
            state.attempts += 1;
            state.plan_attempts = 0;

            let experiences = experience_manager
                .recall(&state.task, self.config.experience_recall_limit as usize)
                .await?;

            if !experiences.is_empty() {
                tracing::info!(
                    "Recalled {} relevant experiences",
                    experiences.len()
                );
            }

            let experience_context = experience_manager.format_recall_context(&experiences);
            let task_with_context = format!("{}{}", state.task, experience_context);

            let plan = match state.review_history.last() {
                Some(review_str) => {
                    let parsed_review = Reviewer::parse_review_response(review_str);
                    if !parsed_review.approved && state.plan_attempts < state.max_plan_attempts {
                        state.plan_attempts += 1;
                        tracing::info!(
                            "Revising plan (attempt {}/{}) based on reviewer feedback",
                            state.plan_attempts, state.max_plan_attempts
                        );
                        let feedback = Self::format_issues_and_suggestions(&parsed_review);
                        tracing::debug!("=== PLANNER INPUT (revision) ===\nTask:\n{}\n\nFeedback:\n{}", task_with_context, feedback);
                        let plan = planner
                            .revise_plan(&task_with_context, state.current_plan.as_ref().unwrap(), &feedback)
                            .await?;
                        tracing::debug!("=== PLANNER OUTPUT ===\n{}", plan);
                        plan
                    } else {
                        state.plan_attempts = 0;
                        tracing::info!("Creating new plan (attempt {}/{})", state.attempts, state.max_attempts);
                        tracing::debug!("=== PLANNER INPUT ===\n{}", task_with_context);
                        let plan = planner.create_plan(&task_with_context).await?;
                        tracing::debug!("=== PLANNER OUTPUT ===\n{}", plan);
                        plan
                    }
                }
                _ => {
                    state.plan_attempts = 0;
                    tracing::info!("Creating new plan (attempt {}/{})", state.attempts, state.max_attempts);
                    tracing::debug!("=== PLANNER INPUT ===\n{}", task_with_context);
                    let plan = planner.create_plan(&task_with_context).await?;
                    tracing::debug!("=== PLANNER OUTPUT ===\n{}", plan);
                    plan
                }
            };

            let plan_id = Uuid::new_v4().to_string();
            state.current_plan = Some(plan.clone());
            state.current_plan_id = Some(plan_id.clone());

            let review = reviewer.review_plan(&state.task, &plan).await?;
            tracing::debug!("=== REVIEWER OUTPUT ===\n{}", review);
            state.review_history.push(review.clone());

            let parsed_review = Reviewer::parse_review_response(&review);
            if !parsed_review.approved {
                tracing::info!(
                    "Plan {} rejected. Issues: {:?}",
                    plan_id,
                    parsed_review.issues
                );

                if state.plan_attempts >= state.max_plan_attempts {
                    tracing::warn!(
                        "Max plan revisions ({}) reached for plan {}",
                        state.max_plan_attempts,
                        plan_id
                    );
                    state.outcome = format!(
                        "Plan could not be approved after {} revisions. Issues: {}",
                        state.max_plan_attempts,
                        parsed_review.issues.join("; ")
                    );
                    state.completed = false;
                    break;
                }

                continue;
            }

            tracing::info!("Plan {} approved. Executing...", plan_id);
            tracing::debug!("=== EXECUTOR INPUT ===\n{}", plan);
            let result = executor.execute_plan(&plan, |event| {
                tracing::debug!("Executor event: {:?}", event);
            }).await?;

            if result.aborted {
                tracing::info!("Execution aborted");
                state.aborted = true;
                state.outcome = "Execution aborted by user".to_string();
                break;
            }

            if result.success {
                tracing::info!("Execution successful on attempt {}", state.attempts);
                state.completed = true;
                let output = result.output.clone();
                state.outcome = output.clone();
                state.execution_output = Self::format_execution_output(&result);

                experience_manager
                    .store_success(&state.task, "general", &output, &result.tools_used, &plan_id)
                    .await?;
                break;
            }

            let error_msg = result.error.clone().unwrap_or_else(|| "Unknown error".to_string());
            tracing::warn!(
                "Execution failed on attempt {}: {}",
                state.attempts,
                error_msg
            );
            state.errors.push(error_msg.clone());

            tracing::debug!("=== CRITIQUE INPUT ===\nTask:\n{}\n\nPlan:\n{}\n\nError:\n{}\n\nTools used: {:?}", state.task, plan, error_msg, result.tools_used);
            let critique_str = reviewer
                .critique_failure(&state.task, &plan, &error_msg, &result.tools_used)
                .await?;

            tracing::debug!("=== CRITIQUE OUTPUT ===\n{}", critique_str);
            let critique = Reviewer::parse_critique_response(&critique_str);

            tracing::info!(
                "Critique parsed: root_cause={}, should_retry={}",
                critique.root_cause,
                critique.should_retry
            );

            state.critique_history.push(critique.clone());

            if !critique.should_retry {
                state.outcome = format!(
                    "Task failed. Root cause: {}. Suggested fix: {}",
                    critique.root_cause, critique.suggested_fix
                );

                experience_manager
                    .store_failure(&state.task, "general", &error_msg, &critique, &result.tools_used, &plan_id)
                    .await?;
                break;
            }

            state.task = format!(
                "{}\n\nPrevious attempt failed: {}\n\nRoot cause: {}\n\nSuggested fix: {}\n\nPlease retry with a different approach.",
                state.original_task,
                error_msg,
                critique.root_cause,
                critique.suggested_fix
            );

            state.review_history.clear();
        }

        if state.attempts >= state.max_attempts && !state.completed {
            state.outcome = format!(
                "Failed after {} attempts. Last error: {}",
                state.attempts,
                state.errors.last().cloned().unwrap_or_default()
            );
        }

        tracing::info!(
            "Evolution complete. attempts={}, completed={}, aborted={}",
            state.attempts,
            state.completed,
            state.aborted
        );

        Ok(state)
    }

    fn format_execution_output(result: &super::executor::ExecutionResult) -> String {
        let mut output = String::new();
        output.push_str("=== Execution Results ===\n\n");

        for (i, step_result) in result.step_results.iter().enumerate() {
            output.push_str(&format!("Step {}: {}\n", i + 1, step_result.step_id));
            output.push_str(&format!("  Tool: {:?}\n", step_result.tool_used));
            if !step_result.output.is_empty() {
                output.push_str(&format!("  Output:\n{}\n", step_result.output));
            }
            if let Some(ref err) = step_result.error {
                output.push_str(&format!("  Error: {}\n", err));
            }
            output.push_str("\n");
        }

        output.push_str(&format!("Steps completed: {}/{}\n", result.steps_completed, result.step_results.len()));
        if !result.tools_used.is_empty() {
            output.push_str(&format!("Tools used: {}\n", result.tools_used.join(", ")));
        }
        output
    }

    fn format_issues_and_suggestions(review: &ReviewResult) -> String {
        let mut feedback = String::new();

        if !review.issues.is_empty() {
            feedback.push_str("Issues identified:\n");
            for issue in &review.issues {
                feedback.push_str(&format!("- {}\n", issue));
            }
        }

        if !review.suggestions.is_empty() {
            feedback.push_str("\nSuggestions:\n");
            for suggestion in &review.suggestions {
                feedback.push_str(&format!("- {}\n", suggestion));
            }
        }

        feedback
    }

    pub async fn run(
        &self,
        task: &str,
        llm: Arc<dyn LlmProvider>,
        mut execute_fn: impl FnMut(&str) -> Result<()>,
    ) -> Result<EvolutionState> {
        let mut state = EvolutionState::new(task.to_string(), self.config.max_attempts);

        while state.attempts < state.max_attempts && !state.completed {
            state.attempts += 1;
            tracing::debug!(
                "Evolution attempt {}/{} for task: {}",
                state.attempts,
                state.max_attempts,
                task
            );

            match execute_fn(&state.task) {
                Ok(()) => {
                    state.completed = true;
                    state.outcome = "Task completed successfully".to_string();
                    tracing::debug!("Task completed successfully on attempt {}", state.attempts);
                    break;
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    tracing::warn!("Task failed on attempt {}: {}", state.attempts, error_msg);
                    state.errors.push(error_msg.clone());

                    if state.attempts >= state.max_attempts {
                        state.outcome = format!(
                            "Failed after {} attempts. Last error: {}",
                            state.attempts, error_msg
                        );
                        break;
                    }

                    let critique = SelfCritique::analyze(&state.task, &error_msg, &[], llm.as_ref()).await?;

                    tracing::debug!(
                        "Critique for attempt {}: root_cause={}, should_retry={}",
                        state.attempts,
                        critique.root_cause,
                        critique.should_retry
                    );

                    state.critique_history.push(critique.clone());

                    if !critique.should_retry {
                        state.outcome = format!(
                            "Critique indicates not to retry. Root cause: {}. Suggested fix: {}",
                            critique.root_cause, critique.suggested_fix
                        );
                        break;
                    }

                    state.task = format!(
                        "{}\n\nPrevious attempt failed with: {}\n\nSuggested fix: {}\n\nPlease retry with this approach.",
                        task,
                        error_msg,
                        critique.suggested_fix
                    );
                }
            }
        }

        Ok(state)
    }

    pub fn should_evolve(&self, error_count: i32) -> bool {
        error_count > 0 && self.config.enabled
    }
}

impl Default for EvolutionGraph {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_evolution_state_new() {
        let state = EvolutionState::new("Test task".to_string(), 5);
        assert_eq!(state.task, "Test task");
        assert_eq!(state.original_task, "Test task");
        assert_eq!(state.max_attempts, 5);
        assert_eq!(state.attempts, 0);
        assert!(!state.completed);
    }

    #[test]
    fn test_evolution_state_with_config() {
        let config = Config::default();
        let state = EvolutionState::with_config("Test task".to_string(), &config);
        assert_eq!(state.max_attempts, config.max_attempts);
        assert_eq!(state.max_plan_attempts, config.max_plan_attempts);
    }

    #[test]
    fn test_plan_step_serialization() {
        let step = PlanStep {
            step_id: 1,
            description: "Read file".to_string(),
            tool: Some("read_file".to_string()),
            parameters: Some(serde_json::json!({"path": "/tmp/test"})),
            depends_on: vec![],
        };

        let json = serde_json::to_string(&step).unwrap();
        let parsed: PlanStep = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.step_id, 1);
        assert_eq!(parsed.description, "Read file");
        assert_eq!(parsed.tool, Some("read_file".to_string()));
    }

    #[test]
    fn test_plan_serialization() {
        let plan = Plan {
            id: "plan_123".to_string(),
            steps: vec![
                PlanStep {
                    step_id: 1,
                    description: "Step 1".to_string(),
                    tool: None,
                    parameters: None,
                    depends_on: vec![],
                },
                PlanStep {
                    step_id: 2,
                    description: "Step 2".to_string(),
                    tool: Some("bash".to_string()),
                    parameters: None,
                    depends_on: vec![1],
                },
            ],
            tools_needed: vec!["bash".to_string()],
            reasoning: "Test reasoning".to_string(),
            based_on_experience: None,
        };

        let json = serde_json::to_string(&plan).unwrap();
        let parsed: Plan = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, "plan_123");
        assert_eq!(parsed.steps.len(), 2);
        assert_eq!(parsed.tools_needed, vec!["bash".to_string()]);
    }
}
