//! GeWu Core - Agent Framework for LLM-Based AI Agents
//!
//! This crate provides the core runtime and abstractions for building
//! LLM-powered AI agents with tool execution capabilities.
//!
//! # Architecture
//!
//! GeWu follows a loop-based execution model where an Agent:
//! 1. Receives user input
//! 2. Maintains conversation history
//! 3. Calls an LLM with tools available
//! 4. Executes any requested tools
//! 5. Continues until a final response is generated
//!
//! # Key Components
//!
//! - [`Agent`] - The main runtime that orchestrates everything
//! - [`Message`] - Conversation messages (system, user, assistant, tool)
//! - [`Tool`] - Trait for implementing tools the agent can call
//! - [`LlmProvider`] - Trait for integrating different LLM backends
//! - [`Memory`] - Trait for conversation history storage
//! - [`Experiences`] - Trait for storing and retrieving past experiences
//!   When configured with [`AgentBuilder::with_experiences`](agent::AgentBuilder::with_experiences),
//!   a [`search_experiences`](agent::SearchExperiencesTool) tool is automatically registered
//!
//! # Crates in the GeWu Workspace
//!
//! - `gewu-core` - This crate, core agent runtime
//! - `gewu-providers` - LLM provider implementations (OpenAI)
//! - `gewu-tools` - Built-in tools (bash, file operations, grep, search, MCP)
//! - `gewu-storage` - Storage backends (SQLite memory)
//! - `gewu-server` - Command-line interface application
//!
//! # Example
//!
//! ```ignore
//! use gewu_core::Agent;
//! use gewu_providers::OpenAIProvider;
//! use std::sync::Arc;
//!
//! let provider = Arc::new(OpenAIProvider::new(&api_key, "gpt-4"));
//!
//! let mut agent = Agent::builder()
//!     .llm(provider)
//!     .with_tools(vec![...])
//!     .system_prompt("You are a helpful assistant.")
//!     .build()
//!     .unwrap();
//! ```

pub mod agent;
pub mod compaction;
pub mod middleware;
pub mod question;
pub mod traits;
pub mod types;

pub use agent::{Agent, AgentTemplate, CancelHandle, RunOptions, Steering};
pub use compaction::{estimate_tokens, is_overflow};
pub use middleware::TruncationService;
pub use question::{Question, QuestionBroker, QuestionOption};
pub use traits::{Experiences, LlmProvider, Memory, SessionStateStore, Tool, ToolRegistry};
pub use types::{
    AgentEvent, ContentPart, Experience, ExperienceQuery, FileAttachment, FunctionCall,
    InMemorySessionStateStore, Message, ProviderEvent, RecalledExperience, SessionState,
    TokenUsage, ToolCall, ToolContext, ToolResult, TruncationResult,
};
