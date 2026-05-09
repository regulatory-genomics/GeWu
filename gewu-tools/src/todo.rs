use crate::fs_utils::into_tool_error;
use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::sync::{Arc, Mutex};

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

pub struct TodoWriteTool {
    pub todos: Arc<Mutex<Vec<TodoItem>>>,
}

pub struct TodoReadTool {
    pub todos: Arc<Mutex<Vec<TodoItem>>>,
}

impl TodoWriteTool {
    pub fn new(todos: Arc<Mutex<Vec<TodoItem>>>) -> Self {
        Self { todos }
    }
}

impl TodoReadTool {
    pub fn new(todos: Arc<Mutex<Vec<TodoItem>>>) -> Self {
        Self { todos }
    }
}

fn lock_err() -> Box<dyn Error + Send + Sync> {
    anyhow::anyhow!("Todo list lock is poisoned").into()
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
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: TodoWriteArgs = serde_json::from_value(args).map_err(into_tool_error)?;

        let mut todos_guard = self.todos.lock().map_err(|_| lock_err())?;
        *todos_guard = input.todos;

        let non_completed = todos_guard
            .iter()
            .filter(|t| t.status != "completed")
            .count();
        Ok(ToolResult::new(
            "todowrite",
            format!("{non_completed} todos"),
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
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let todos_guard = self.todos.lock().map_err(|_| lock_err())?;
        let non_completed = todos_guard
            .iter()
            .filter(|t| t.status != "completed")
            .count();
        Ok(ToolResult::new(
            "todoread",
            format!(
                "{} todos\n\n{}",
                non_completed,
                serde_json::to_string_pretty(&*todos_guard).map_err(into_tool_error)?
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_todo_write_and_read() {
        let todos = Arc::new(Mutex::new(Vec::<TodoItem>::new()));
        let write_tool = TodoWriteTool::new(todos.clone());
        let read_tool = TodoReadTool::new(todos.clone());

        let input = serde_json::json!({
            "todos": [
                {"id": "1", "content": "Write code", "status": "in_progress", "priority": 1},
                {"id": "2", "content": "Write tests", "status": "pending", "priority": 2},
                {"id": "3", "content": "Done task", "status": "completed", "priority": 3}
            ]
        });

        let ctx = ToolContext::new("test", "msg");
        let result = write_tool.execute(input, &ctx).await.unwrap();
        assert_eq!(result.output, "2 todos");

        let read_result = read_tool
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(read_result.output.contains("2 todos"));
    }
}
