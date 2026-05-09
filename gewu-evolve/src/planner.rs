//! Planner agent for creating and revising execution plans.

use anyhow::Result;
use futures::StreamExt;
use gewu_core::Agent;

pub struct Planner {
    agent: Agent,
}

impl Planner {
    pub fn new(agent: Agent) -> Self {
        Self { agent }
    }

    pub async fn create_plan(&mut self, task: &str) -> Result<String> {
        let prompt = Self::build_plan_prompt(task, None, None);
        self.agent.reset().await;
        self.collect_response(&prompt).await
    }

    pub async fn revise_plan(
        &mut self,
        task: &str,
        existing_plan: &str,
        feedback: &str,
    ) -> Result<String> {
        let prompt = Self::build_plan_prompt(task, Some(existing_plan), Some(feedback));
        self.agent.reset().await;
        self.collect_response(&prompt).await
    }

    async fn collect_response(&mut self, prompt: &str) -> Result<String> {
        let mut stream = Box::pin(self.agent.ask(prompt));
        let mut response_text = String::new();

        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => match event {
                    gewu_core::types::AgentEvent::ContentDelta { delta } => {
                        response_text.push_str(&delta);
                    }
                    gewu_core::types::AgentEvent::Finish { .. } => break,
                    _ => {}
                },
                Err(e) => {
                    tracing::warn!("Error in planner stream: {}", e);
                    break;
                }
            }
        }

        Ok(response_text)
    }

    fn build_plan_prompt(
        task: &str,
        existing_plan: Option<&str>,
        feedback: Option<&str>,
    ) -> String {
        let task_section = format!(
            r#"Task to accomplish:
{}"#,
            task
        );

        let revision_section = match (existing_plan, feedback) {
            (Some(plan), Some(review)) => format!(
                "\n\nPrevious Plan:\n---\n\n{}\n\n---\n\nReviewer Feedback:\n{}\n\nPlease revise the plan to address the feedback.",
                plan, review
            ),
            _ => String::new(),
        };

        format!(
            r#"You are a planner agent creating an execution plan.

Your task is to break down the user's request into clear, sequential steps.

When planning, you may use available tools to:
- Read existing files to understand the codebase structure
- Search for relevant code patterns
- Look up documentation or references

However, you should NOT execute bash commands or modify files during planning.

Create clear, sequential plans that can be executed by a separate executor agent.

{}
{}

Respond with:
1. A numbered list of steps (Step 1, Step 2, etc.)
2. Brief reasoning at the end explaining your approach

Format your response clearly so it can be understood by an executor agent."#,
            task_section, revision_section
        )
    }
}