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
//! | [`BashTool`] | `bash` | Execute shell commands |
//! | [`ApplyPatchTool`] | `apply_patch` | Apply validated multi-file patches |
//! | [`ReadFileTool`] | `read_file` | Read files and directories |
//! | [`WriteFileTool`] | `write_file` | Write/create files |
//! | [`EditFileTool`] | `edit_file` | Edit files by replacing text |
//! | [`GlobTool`] | `glob` | Find files by glob patterns |
//! | [`GrepTool`] | `grep` | Search file contents |
//! | [`ListTool`] | `list` | List directory contents |
//! | [`WebFetchTool`] | `webfetch` | Fetch web pages |
//! | [`WebSearchTool`] | `websearch` | Search the web |
//! | [`CodeSearchTool`] | `codesearch` | Search code semantically |
//! | [`TodoReadTool`] | `todoread` | Read the task list |
//! | [`TodoWriteTool`] | `todowrite` | Write the task list |
//! | [`TaskTool`] | `task` | Launch sub-agents for complex tasks |
//! | [`WorkflowStartTool`] | `workflow_start` | Start a background workflow run from a JSON spec |
//! | [`WorkflowStatusTool`] | `workflow_status` | Snapshot a workflow run without waiting |
//! | [`WorkflowCancelTool`] | `workflow_cancel` | Abort a workflow run |
//! | [`SkillTool`] | `skill` | Load skill instructions into the conversation |
//! | [`QuestionTool`] | `question` | Ask the user structured questions mid-run |
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

pub mod apply_patch;
pub mod bash;
pub mod fs;
pub mod fs_utils;
pub mod grep;
pub mod list;
pub mod mcp;
mod media;
pub mod question;
pub mod replacers;
pub mod search;
pub mod shell_env;
pub mod skill;
pub mod task;
pub mod todo;
pub mod web_fetch;
pub mod workflow;

pub use apply_patch::ApplyPatchTool;
pub use bash::BashTool;
pub use fs::{EditFileTool, GlobTool, ReadFileTool, WriteFileTool};
pub use grep::GrepTool;
pub use list::ListTool;
pub use media::{DEFAULT_MAX_BASE64_BYTES, DEFAULT_MAX_DIMENSION, ImageLimits};
pub use question::QuestionTool;
pub use search::{CodeSearchTool, WebSearchTool};
pub use shell_env::{ShellEnvironment, ShellError, ShellLimits, ShellMount, ShellOutcome};
pub use skill::{SkillInfo, SkillTool};
pub use task::{AgentFactory, AgentOverrides, TaskTool};
pub use todo::{TodoItem, TodoReadTool, TodoWriteTool};
pub use web_fetch::{WebFetchPolicy, WebFetchTool};
pub use workflow::{WorkflowCancelTool, WorkflowManager, WorkflowStartTool, WorkflowStatusTool};
