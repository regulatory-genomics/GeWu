use crate::fs_utils::into_tool_error;
use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use std::error::Error;

/// Session-state key under which the todo list is stored.
const TODOS_KEY: &str = "todos";

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: String,
    pub priority: Option<u8>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TodoWriteArgs {
    pub todos: Vec<TodoItem>,
}

/// Writes the session's todo list through [`ToolContext::session_state`],
/// so the list persists per session and the tool itself stays stateless.
/// Echoes the full updated list back (the opencode v2 shape), so no
/// separate read tool is needed.
#[derive(Debug, Default, Clone, Copy)]
pub struct TodoWriteTool;

impl TodoWriteTool {
    pub fn new() -> Self {
        Self
    }
}

fn non_completed_count(todos: &[TodoItem]) -> usize {
    todos
        .iter()
        .filter(|todo| todo.status != "completed")
        .count()
}

fn render_todo_summary(todos: &[TodoItem]) -> Result<String, serde_json::Error> {
    Ok(format!(
        "{} todos\n\n{}",
        non_completed_count(todos),
        serde_json::to_string_pretty(todos)?
    ))
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todowrite"
    }

    fn description(&self) -> &str {
        "Use this tool to update your todo list with the current tasks and their status"
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(TodoWriteArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: TodoWriteArgs = serde_json::from_value(args).map_err(into_tool_error)?;

        ctx.session_state
            .write(&ctx.session_id, TODOS_KEY, &input.todos)
            .await
            .map_err(into_tool_error)?;

        Ok(ToolResult::new(
            "todowrite",
            render_todo_summary(&input.todos).map_err(into_tool_error)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_core::types::SessionState;

    #[test]
    fn summary_preserves_status_semantics_and_serialization() {
        assert_eq!(render_todo_summary(&[]).unwrap(), "0 todos\n\n[]");
        let todos: Vec<TodoItem> = serde_json::from_value(serde_json::json!([
            {"id":"2", "content":"你好", "status":"completed", "priority":null},
            {"id":"1", "content":"next\nstep", "status":"Completed", "priority":1},
            {"id":"3", "content":"", "status":"", "priority":null}
        ]))
        .unwrap();
        assert_eq!(non_completed_count(&todos), 2);
        assert_eq!(
            render_todo_summary(&todos).unwrap(),
            format!(
                "2 todos\n\n{}",
                serde_json::to_string_pretty(&todos).unwrap()
            )
        );
        assert_eq!(non_completed_count(&todos[..1]), 0);
    }

    #[tokio::test]
    async fn test_todo_write_echoes_full_list_and_persists() {
        let state = SessionState::in_memory();
        let write_tool = TodoWriteTool;

        let input = serde_json::json!({
            "todos": [
                {"id": "1", "content": "Write code", "status": "in_progress", "priority": 1},
                {"id": "2", "content": "Write tests", "status": "pending", "priority": 2},
                {"id": "3", "content": "Done task", "status": "completed", "priority": 3}
            ]
        });

        let ctx = ToolContext::new("test", "msg").with_session_state(state.clone());
        let result = write_tool.execute(input, &ctx).await.unwrap();
        assert!(result.output.starts_with("2 todos"), "{}", result.output);
        assert!(
            result.output.contains("Write code"),
            "the echo must contain the updated list: {}",
            result.output
        );

        let stored: Vec<TodoItem> = state.read("test", TODOS_KEY).await.unwrap().unwrap();
        assert_eq!(stored.len(), 3);
        assert_eq!(stored[0].content, "Write code");
    }

    #[tokio::test]
    async fn test_todo_lists_are_isolated_per_session() {
        let state = SessionState::in_memory();
        let write_tool = TodoWriteTool;

        let ctx = ToolContext::new("session-a", "msg").with_session_state(state.clone());
        let input = serde_json::json!({
            "todos": [
                {"id": "1", "content": "Only for A", "status": "pending", "priority": null}
            ]
        });
        write_tool.execute(input, &ctx).await.unwrap();

        let other_ctx = ToolContext::new("session-b", "msg").with_session_state(state.clone());
        let result = write_tool
            .execute(serde_json::json!({"todos": []}), &other_ctx)
            .await
            .unwrap();
        assert_eq!(result.output, "0 todos\n\n[]");

        // Session B's empty write must not have touched session A's list.
        let restored: Vec<TodoItem> = state.read("session-a", TODOS_KEY).await.unwrap().unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].content, "Only for A");
    }
}
