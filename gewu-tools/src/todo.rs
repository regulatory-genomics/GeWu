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

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Default)]
pub struct TodoReadArgs {
    #[serde(default)]
    pub _placeholder: Option<String>,
}

/// Writes the session's todo list through [`ToolContext::session_state`],
/// so the list persists per session and the tool itself stays stateless.
#[derive(Debug, Default, Clone, Copy)]
pub struct TodoWriteTool;

/// Reads the session's todo list from [`ToolContext::session_state`].
#[derive(Debug, Default, Clone, Copy)]
pub struct TodoReadTool;

impl TodoWriteTool {
    pub fn new() -> Self {
        Self
    }
}

impl TodoReadTool {
    pub fn new() -> Self {
        Self
    }
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

        let non_completed = input
            .todos
            .iter()
            .filter(|t| t.status != "completed")
            .count();
        Ok(ToolResult::new(
            "todowrite",
            format!("{} todos", non_completed),
        ))
    }
}

#[async_trait]
impl Tool for TodoReadTool {
    fn name(&self) -> &str {
        "todoread"
    }

    fn description(&self) -> &str {
        "Use this tool to read your todo list"
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(TodoReadArgs)
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let todos: Vec<TodoItem> = ctx
            .session_state
            .read(&ctx.session_id, TODOS_KEY)
            .await
            .map_err(into_tool_error)?
            .unwrap_or_default();

        let non_completed = todos.iter().filter(|t| t.status != "completed").count();
        Ok(ToolResult::new(
            "todoread",
            format!(
                "{} todos\n\n{}",
                non_completed,
                serde_json::to_string_pretty(&todos).map_err(into_tool_error)?
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_core::types::SessionState;

    #[tokio::test]
    async fn test_todo_write_and_read() {
        let state = SessionState::in_memory();
        let write_tool = TodoWriteTool;
        let read_tool = TodoReadTool;

        let input = serde_json::json!({
            "todos": [
                {"id": "1", "content": "Write code", "status": "in_progress", "priority": 1},
                {"id": "2", "content": "Write tests", "status": "pending", "priority": 2},
                {"id": "3", "content": "Done task", "status": "completed", "priority": 3}
            ]
        });

        let ctx = ToolContext::new("test", "msg").with_session_state(state.clone());
        let result = write_tool.execute(input, &ctx).await.unwrap();
        assert_eq!(result.output, "2 todos");

        let read_ctx = ToolContext::new("test", "msg").with_session_state(state.clone());
        let read_result = read_tool
            .execute(serde_json::json!({}), &read_ctx)
            .await
            .unwrap();
        assert!(read_result.output.contains("2 todos"));
    }

    #[tokio::test]
    async fn test_todo_lists_are_isolated_per_session() {
        let state = SessionState::in_memory();
        let write_tool = TodoWriteTool;
        let read_tool = TodoReadTool;

        let ctx = ToolContext::new("session-a", "msg").with_session_state(state.clone());
        let input = serde_json::json!({
            "todos": [
                {"id": "1", "content": "Only for A", "status": "pending", "priority": null}
            ]
        });
        write_tool.execute(input, &ctx).await.unwrap();

        let other_ctx = ToolContext::new("session-b", "msg").with_session_state(state.clone());
        let read_result = read_tool
            .execute(serde_json::json!({}), &other_ctx)
            .await
            .unwrap();
        assert!(read_result.output.contains("0 todos"));
    }
}
