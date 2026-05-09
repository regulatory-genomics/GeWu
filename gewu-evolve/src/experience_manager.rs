//! Experience Manager for coordinating experience storage and recall.

use crate::critique::CritiqueResult;
use anyhow::Result;
use std::sync::Arc;

pub struct ExperienceManager {
    store: Option<Arc<gewu_experience::ExperienceStore>>,
}

impl ExperienceManager {
    pub fn new(store: Option<Arc<gewu_experience::ExperienceStore>>) -> Self {
        Self { store }
    }

    pub async fn recall(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<gewu_experience::RecalledExperience>> {
        match &self.store {
            Some(store) => {
                let experience_query = gewu_experience::ExperienceQuery {
                    query_text: query.to_string(),
                    task_type_filter: None,
                    limit,
                    score_threshold: 0.5,
                };
                store.recall(&experience_query).await.map_err(|e| anyhow::anyhow!(e))
            }
            None => Ok(Vec::new()),
        }
    }

    pub async fn store_success(
        &self,
        task: &str,
        task_type: &str,
        result: &str,
        tools_used: &[String],
        _plan_id: &str,
    ) -> Result<()> {
        match &self.store {
            Some(store) => {
                let store_result = store
                    .smart_store(task, task_type, result, tools_used, true)
                    .await;
                match store_result {
                    Ok(r) => tracing::info!("Stored success experience: {}", r),
                    Err(e) => tracing::warn!("Failed to store success experience: {}", e),
                }
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub async fn store_failure(
        &self,
        task: &str,
        task_type: &str,
        error: &str,
        critique: &CritiqueResult,
        tools_used: &[String],
        _plan_id: &str,
    ) -> Result<()> {
        match &self.store {
            Some(store) => {
                let failure_summary = format!(
                    "Error: {}\nRoot Cause: {}\nSuggested Fix: {}",
                    error, critique.root_cause, critique.suggested_fix
                );

                let store_result = store
                    .smart_store(task, task_type, &failure_summary, tools_used, false)
                    .await;

                match store_result {
                    Ok(r) => tracing::info!("Stored failure experience: {}", r),
                    Err(e) => tracing::warn!("Failed to store failure experience: {}", e),
                }
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub fn is_available(&self) -> bool {
        self.store.is_some()
    }

    pub fn format_recall_context(&self, experiences: &[gewu_experience::RecalledExperience]) -> String {
        if experiences.is_empty() {
            return String::new();
        }

        let mut context = String::from("\n\n[Relevant past experiences for this task]\n");
        for (i, recalled) in experiences.iter().enumerate() {
            let exp = &recalled.experience;
            context.push_str(&format!(
                "{}. [{}] {}\n   Solution: {}\n   Tools: {}\n",
                i + 1,
                exp.task_type,
                exp.task_description,
                exp.solution_summary,
                exp.tools_used.join(", ")
            ));
        }
        context.push_str("Use these experiences to inform your approach.\n");
        context.push_str("---\n\n");
        context
    }
}

impl Default for ExperienceManager {
    fn default() -> Self {
        Self::new(None)
    }
}
