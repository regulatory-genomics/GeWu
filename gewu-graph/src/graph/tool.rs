//! GraphTool adapter for integrating graphs with the Tool interface.
//!
//! This module provides `GraphTool` which wraps a `CompiledGraph` to implement
//! the `Tool` trait from `gewu_core`. This allows graphs to be used as tools
//! within agent workflows.

use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::Serialize;
use std::error::Error;

/// A wrapper that converts a `CompiledGraph` into a `Tool`.
///
/// `GraphTool` allows graphs to be executed as tools within the GeWu
/// agent framework. It implements the `Tool` trait, providing:
///
/// - Tool name and description
/// - JSON schema for the input state type
/// - Async execution via `execute()`
///
/// # Type Parameters
///
/// * `S` - The state type that this graph operates on, must implement
///   `State + schemars::JsonSchema + Send + Sync`
pub struct GraphTool<S: crate::State> {
    name: String,
    description: String,
    graph: crate::CompiledGraph<S>,
}

impl<S: crate::State> GraphTool<S> {
    /// Creates a new `GraphTool` with the given name, description, and compiled graph.
    ///
    /// # Arguments
    /// * `name` - The tool's identifier
    /// * `description` - Human-readable description of what the tool does
    /// * `graph` - A compiled graph ready for execution
    pub fn new(name: &str, description: &str, graph: crate::CompiledGraph<S>) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            graph,
        }
    }

    /// Returns the tool's name.
    pub fn tool_name(&self) -> &str {
        &self.name
    }

    /// Returns the tool's description.
    pub fn tool_description(&self) -> &str {
        &self.description
    }

    /// Returns the tool's name (alias for `tool_name`).
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[async_trait]
impl<S: crate::State + schemars::JsonSchema + Send + Sync + 'static> Tool for GraphTool<S>
where
    S: 'static,
    S::Partial: Serialize,
{
    /// Returns the tool's name.
    fn name(&self) -> &str {
        &self.name
    }

    /// Returns the tool's description.
    fn description(&self) -> &str {
        &self.description
    }

    /// Returns the JSON schema for the input state type.
    ///
    /// This schema is generated from the state type `S` and can be used
    /// by LLM providers to validate and understand tool inputs.
    fn schema(&self) -> RootSchema {
        schemars::schema_for!(S)
    }

    /// Executes the graph with the provided input state.
    ///
    /// This calls the underlying graph's `invoke()` method synchronously.
    /// The input is deserialized from JSON, the graph executes, and the
    /// result is serialized back to JSON in a `ToolResult`.
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: S = serde_json::from_value(args)?;
        let result = self.graph.invoke(input)?;
        Ok(ToolResult::new(&self.name, serde_json::to_string(&result)?))
    }
}
