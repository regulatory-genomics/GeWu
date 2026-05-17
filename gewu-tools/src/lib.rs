//! GeWu Tools - Built-in tools for the GeWu agent framework.
//!
//! This crate provides a collection of built-in tools that extend the
//! agent's capabilities. Tools are the primary way for the agent to
//! interact with the filesystem, execute commands, search code, and more.
//!
//! # Available Tools
//!
//! | Tool | Name | Description |
//! |------|------|-------------|
//! | [`BashTool`](bash::BashTool) | `bash` | Execute shell commands |
//! | [`ReadFileTool`](fs::ReadFileTool) | `read_file` | Read files and directories |
//! | [`WriteFileTool`](fs::WriteFileTool) | `write_file` | Write/create files |
//! | [`EditFileTool`](fs::EditFileTool) | `edit_file` | Edit files by replacing text |
//! | [`GlobTool`](fs::GlobTool) | `glob` | Find files by glob patterns |
//! | [`GrepTool`](grep::GrepTool) | `grep` | Search file contents |
//! | [`ListTool`](list::ListTool) | `list` | List directory contents |
//! | [`WebFetchTool`](web_fetch::WebFetchTool) | `web_fetch` | Fetch web pages |
//! | [`WebSearchTool`](search::WebSearchTool) | `web_search` | Search the web |
//! | [`TodoReadTool`](todo::TodoReadTool) | `todo_read` | Read the task list |
//! | [`TodoWriteTool`](todo::TodoWriteTool) | `todo_write` | Write the task list |
//! | [`BatchTool`](batch::BatchTool) | `batch` | Execute multiple tools at once |
//!
//! # MCP Tools
//!
//! The module also supports [Model Context Protocol (MCP)](mcp) tools,
//! allowing integration with external tool servers.
//!
//! # Creating Custom Tools
//!
//! Implement the [`Tool`](gewu_core::traits::Tool) trait to create custom tools:
//!
//! ```ignore
//! use async_trait::async_trait;
//! use gewu_core::traits::Tool;
//! use serde_json::json;
//!
//! struct MyTool;
//!
//! #[async_trait]
//! impl Tool for MyTool {
//!     fn name(&self) -> &str { "my_tool" }
//!     fn description(&self) -> &str { "Does something useful" }
//!     fn parameters_schema(&self) -> Value {
//!         json!({ ... })
//!     }
//!     async fn call(&self, args: &str) -> Result<String, Box<dyn Error + Send + Sync>> {
//!         // Implementation
//!         Ok("result".to_string())
//!     }
//! }
//! ```

pub mod bash;
pub mod batch;
pub mod fs;
pub mod fs_utils;
pub mod grep;
pub mod list;
pub mod mcp;
pub mod replacers;
pub mod report_result;
pub mod response;
pub mod search;
pub mod todo;
pub mod web_fetch;

pub use bash::BashTool;
pub use batch::BatchTool;
pub use fs::{EditFileTool, GlobTool, ReadFileTool, WriteFileTool};
pub use grep::GrepTool;
pub use list::ListTool;
pub use report_result::ReportResultTool;
pub use search::{CodeSearchTool, WebSearchTool};
pub use todo::{TodoItem, TodoReadTool, TodoWriteTool};
pub use web_fetch::WebFetchTool;
