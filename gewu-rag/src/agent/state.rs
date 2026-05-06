use gewu_core::Message;
use gewu_graph::state::State;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RagQueryResult {
    pub index: usize,
    pub question: String,
    pub answer: String,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentState {
    pub tool_call_count: i32,
    pub iteration_count: i32,
    pub question: String,
    pub question_index: usize,
    pub context_summary: String,
    pub retrieval_keys: Vec<String>,
    pub final_answer: String,
    pub agent_answers: Vec<RagQueryResult>,
    pub retrieved_data: Vec<String>,
    pub messages: Vec<Message>,
}

impl State for AgentState {
    type Partial = PartialAgentState;

    fn merge(&self, update: Self::Partial) -> Self {
        let _old_tool_count = self.tool_call_count;
        let _update_tool_count = update.tool_call_count;

        let mut combined_keys = self.retrieval_keys.clone();
        if let Some(new_keys) = update.retrieval_keys {
            for key in new_keys {
                if !combined_keys.contains(&key) {
                    combined_keys.push(key);
                }
            }
        }

        let mut combined_answers = self.agent_answers.clone();
        if let Some(new_answers) = update.agent_answers {
            for answer in new_answers {
                if !combined_answers.iter().any(|a| a.index == answer.index) {
                    combined_answers.push(answer);
                }
            }
        }

        let new_tool_count = update
            .tool_call_count
            .map(|u| self.tool_call_count + u)
            .unwrap_or(self.tool_call_count);
        let new_iteration_count = update
            .iteration_count
            .map(|u| self.iteration_count + u)
            .unwrap_or(self.iteration_count);

        tracing::debug!(
            "[STATE_MERGE] tool_call_count: {} + {:?} -> {}",
            self.tool_call_count,
            update.tool_call_count,
            new_tool_count
        );

        Self {
            tool_call_count: new_tool_count,
            iteration_count: new_iteration_count,
            question: update.question.unwrap_or_else(|| self.question.clone()),
            question_index: update.question_index.unwrap_or(self.question_index),
            context_summary: update
                .context_summary
                .unwrap_or_else(|| self.context_summary.clone()),
            retrieval_keys: combined_keys,
            final_answer: update
                .final_answer
                .unwrap_or_else(|| self.final_answer.clone()),
            agent_answers: combined_answers,
            retrieved_data: update
                .retrieved_data
                .unwrap_or_else(|| self.retrieved_data.clone()),
            messages: update.messages.unwrap_or_else(|| self.messages.clone()),
        }
    }
}

impl AgentState {
    pub fn new(question: String, question_index: usize) -> Self {
        Self {
            question,
            question_index,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PartialAgentState {
    pub tool_call_count: Option<i32>,
    pub iteration_count: Option<i32>,
    pub question: Option<String>,
    pub question_index: Option<usize>,
    pub context_summary: Option<String>,
    pub retrieval_keys: Option<Vec<String>>,
    pub final_answer: Option<String>,
    pub agent_answers: Option<Vec<RagQueryResult>>,
    pub retrieved_data: Option<Vec<String>>,
    pub messages: Option<Vec<Message>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MainState {
    pub question_is_clear: bool,
    pub conversation_summary: String,
    pub original_query: String,
    pub rewritten_questions: Vec<String>,
    pub agent_answers: Vec<RagQueryResult>,
    pub messages: Vec<Message>,
    pub clarification_needed: Option<String>,
}

impl State for MainState {
    type Partial = PartialMainState;

    fn merge(&self, update: Self::Partial) -> Self {
        Self {
            question_is_clear: update.question_is_clear.unwrap_or(self.question_is_clear),
            conversation_summary: update
                .conversation_summary
                .unwrap_or_else(|| self.conversation_summary.clone()),
            original_query: update
                .original_query
                .unwrap_or_else(|| self.original_query.clone()),
            rewritten_questions: update
                .rewritten_questions
                .unwrap_or_else(|| self.rewritten_questions.clone()),
            agent_answers: update
                .agent_answers
                .unwrap_or_else(|| self.agent_answers.clone()),
            messages: update.messages.unwrap_or_else(|| self.messages.clone()),
            clarification_needed: update.clarification_needed.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PartialMainState {
    pub question_is_clear: Option<bool>,
    pub conversation_summary: Option<String>,
    pub original_query: Option<String>,
    pub rewritten_questions: Option<Vec<String>>,
    pub agent_answers: Option<Vec<RagQueryResult>>,
    pub clarification_needed: Option<String>,
    pub messages: Option<Vec<Message>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_state_default() {
        let state = AgentState::default();
        assert_eq!(state.tool_call_count, 0);
        assert_eq!(state.iteration_count, 0);
        assert!(state.question.is_empty());
        assert!(state.agent_answers.is_empty());
        assert!(state.retrieval_keys.is_empty());
    }

    #[test]
    fn test_agent_state_new() {
        let state = AgentState::new("test question".to_string(), 1);
        assert_eq!(state.question, "test question");
        assert_eq!(state.question_index, 1);
        assert_eq!(state.tool_call_count, 0);
    }

    #[test]
    fn test_main_state_default() {
        let state = MainState::default();
        assert!(!state.question_is_clear);
        assert!(state.conversation_summary.is_empty());
        assert!(state.rewritten_questions.is_empty());
        assert!(state.agent_answers.is_empty());
        assert!(state.clarification_needed.is_none());
    }

    #[test]
    fn test_partial_agent_state_merge() {
        let state = AgentState::default();
        let partial = PartialAgentState {
            tool_call_count: Some(5),
            question: Some("updated question".to_string()),
            ..Default::default()
        };

        let merged = state.merge(partial);
        assert_eq!(merged.tool_call_count, 5);
        assert_eq!(merged.question, "updated question");
        assert_eq!(merged.iteration_count, 0);
    }

    #[test]
    fn test_partial_main_state_merge() {
        let state = MainState::default();
        let partial = PartialMainState {
            question_is_clear: Some(true),
            original_query: Some("original".to_string()),
            ..Default::default()
        };

        let merged = state.merge(partial);
        assert!(merged.question_is_clear);
        assert_eq!(merged.original_query, "original");
    }

    #[test]
    fn test_rag_query_result() {
        let result = RagQueryResult {
            index: 0,
            question: "test".to_string(),
            answer: "answer".to_string(),
            sources: vec!["doc.pdf".to_string()],
        };

        assert_eq!(result.index, 0);
        assert_eq!(result.question, "test");
        assert_eq!(result.answer, "answer");
        assert_eq!(result.sources.len(), 1);
    }
}
