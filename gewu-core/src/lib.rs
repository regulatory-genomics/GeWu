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
//!
//! # Crates in the GeWu Workspace
//!
//! - `gewu-core` - This crate, core agent runtime
//! - `gewu-providers` - LLM provider implementations (OpenAI)
//! - `gewu-tools` - Built-in tools (bash, file operations, grep, search, MCP)
//! - `gewu-storage` - Storage backends (SQLite memory)
//! - `gewu-cli` - Command-line interface application
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
pub mod traits;
pub mod types;
pub mod work_graph;

pub use agent::Agent;
pub use traits::{
    AssemblyContext, LlmProvider, Memory, Plugin, PluginPlatformContext, Tool, ToolFactory,
    ToolProvider, ToolRegistry,
};
pub use types::{
    CapabilityDescriptor, FileAttachment, Message, SpecialistDescriptor,
    SpecialistExecutionEnvelope, SpecialistExecutionRequest, ToolContext, ToolExecutionRecord,
    ToolResult, TruncationResult,
};
pub use work_graph::{
    WorkArtifact, WorkEdge, WorkEdgeKind, WorkEvent, WorkEventKind, WorkGraph, WorkNode,
    WorkNodeKind, WorkStatus,
};
