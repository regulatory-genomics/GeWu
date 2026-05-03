//! Core traits for the GeWu agent framework.
//!
//! This module defines the fundamental traits that must be implemented to
//! extend GeWu with custom tools, LLM providers, and memory backends:
//!
//! - [`Tool`] - Implement to create custom tools that the agent can call
//! - [`LlmProvider`] - Implement to add support for different LLM backends
//! - [`ToolRegistry`] - Implement to provide a collection of tools to the agent
//! - [`Memory`] - Implement to add custom conversation history storage
//!
//! # Example: Implementing a Custom Tool
//!
//! ```ignore
//! use async_trait::async_trait;
//! use gewu_core::traits::Tool;
//! use gewu_core::types::{ToolResult, ToolContext};
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(JsonSchema, Deserialize)]
//! struct MyParams {
//!     input: String,
//! }
//!
//! struct MyTool;
//!
//! #[async_trait]
//! impl Tool for MyTool {
//!     fn name(&self) -> &str { "my_tool" }
//!
//!     fn description(&self) -> &str { "A description of what my tool does" }
//!
//!     fn schema() -> schemars::schema::RootSchema {
//!         schemars::schema_for!(MyParams)
//!     }
//!
//!     async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
//!         let params: MyParams = serde_json::from_value(args)?;
//!         Ok(ToolResult::new("my_tool", format!("result: {}", params.input)))
//!     }
//! }
//! ```

use crate::types::{
    CapabilityDescriptor, Message, ProviderEvent, SpecialistDescriptor, ToolContext,
    ToolExecutionPolicy, ToolResult,
};
use async_trait::async_trait;
use futures::stream::BoxStream;
use schemars::schema::RootSchema;
use serde_json::Value;
use sqlx::SqlitePool;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    fn description(&self) -> &str;

    fn schema(&self) -> RootSchema;

    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::Serial
    }

    fn start_metadata(
        &self,
        _args: &str,
        _ctx: &ToolContext,
    ) -> Result<serde_json::Value, Box<dyn Error + Send + Sync>> {
        Ok(serde_json::Value::Null)
    }

    async fn init(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>>;
}

#[async_trait]
pub trait ToolRegistry: Send + Sync {
    async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>>;

    fn list_tools(&self) -> Vec<String>;
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn init(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>
    where
        Self: Sized,
    {
        Ok(())
    }

    async fn stream_response<'a>(
        &'a self,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    >;

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>>;
}

#[async_trait]
pub trait Memory: Send + Sync {
    async fn init(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>
    where
        Self: Sized,
    {
        Ok(())
    }

    fn messages(&self) -> &[Message];

    async fn add_message(&mut self, message: Message) -> Result<(), Box<dyn Error + Send + Sync>>;

    async fn clear(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>;

    fn messages_mut(&mut self) -> &mut [Message];

    async fn replace(&mut self, messages: Vec<Message>)
    -> Result<(), Box<dyn Error + Send + Sync>>;
}

#[async_trait]
impl Memory for Vec<Message> {
    async fn init(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>
    where
        Self: Sized,
    {
        Ok(())
    }

    fn messages(&self) -> &[Message] {
        self
    }

    async fn add_message(&mut self, message: Message) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.push(message);
        Ok(())
    }

    async fn clear(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.clear();
        Ok(())
    }

    fn messages_mut(&mut self) -> &mut [Message] {
        self.as_mut_slice()
    }

    async fn replace(
        &mut self,
        messages: Vec<Message>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        *self = messages;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Plugin system traits
// ---------------------------------------------------------------------------

/// Context passed to tool factories during specialist assembly.
///
/// Contains the shared resources that tool constructors typically need.
pub struct AssemblyContext {
    pub working_directory: PathBuf,
    pub provider: Arc<dyn LlmProvider>,
    pub pool: SqlitePool,
    pub metadata_json: Value,
}

/// Platform-level context provided to plugins during initialization.
///
/// Contains shared infrastructure resources and plugin-specific configuration
/// so plugins don't need to reach into global environment state.
pub struct PluginPlatformContext {
    pub provider: Arc<dyn LlmProvider>,
    pub pool: SqlitePool,
    pub working_directory: PathBuf,
    pub plugin_config: Value,
}

/// A factory that produces tool instances from capability names.
///
/// Implement this trait to provide custom tools that can be assembled into
/// specialists. Each factory declares which capability names it handles and
/// creates the corresponding tool instances on demand.
pub trait ToolFactory: Send + Sync {
    /// The capability names this factory handles (e.g., `["count_lines"]`).
    fn capabilities(&self) -> Vec<String>;

    /// Create tool instances for the given capability name.
    fn create_tools(
        &self,
        capability: &str,
        ctx: &AssemblyContext,
    ) -> Result<Vec<Box<dyn Tool>>, Box<dyn Error + Send + Sync>>;
}

/// A unified source of tools, keyed by capability id.
///
/// `ToolProvider` is a strict superset of `ToolFactory`: in addition to naming
/// capabilities, it supplies rich `CapabilityDescriptor`s (kind, description,
/// schema, metadata). This is the abstraction used by `CapabilityRegistry` to
/// resolve capabilities declared in manifests.
///
/// Any `ToolFactory` can be adapted via
/// `gewu_runtime::capability::ToolFactoryProvider`.
pub trait ToolProvider: Send + Sync {
    /// A short identifier for the provider kind:
    /// "builtin" | "plugin" | "cli" | "script" | "mcp" | "http" | "python"
    /// | "container" | "database" | "graph".
    fn kind(&self) -> &str;

    /// Optional stable id for this provider instance (used for diagnostics and
    /// namespacing). Defaults to the kind.
    fn id(&self) -> &str {
        self.kind()
    }

    /// The capabilities this provider handles.
    fn capabilities(&self) -> Vec<CapabilityDescriptor>;

    /// Build concrete tool(s) for a capability name. Returning an empty vec
    /// signals the capability is not handled.
    fn build_tools(
        &self,
        capability: &str,
        ctx: &AssemblyContext,
    ) -> Result<Vec<Box<dyn Tool>>, Box<dyn Error + Send + Sync>>;
}

/// A plugin that provides tool factories and/or specialist descriptors.
///
/// This is the single integration point for external crates. Implement this
/// trait and pass it to `GeWuRuntime::builder().register_plugin(...)`.
pub trait Plugin: Send + Sync {
    /// A human-readable name for this plugin.
    fn name(&self) -> &str;

    /// Tool factories provided by this plugin.
    fn tool_factories(&self) -> Vec<Box<dyn ToolFactory>>;

    /// Specialist descriptors provided by this plugin.
    fn specialist_descriptors(&self) -> Vec<SpecialistDescriptor>;

    /// Optional directory containing YAML specialist manifests bundled with
    /// this plugin. Loaded by `CapabilityRegistry` / `SpecialistManifestLoader`
    /// when the runtime is built. Returning `None` means the plugin ships no
    /// manifests (the default).
    fn specialist_manifest_dir(&self) -> Option<PathBuf> {
        None
    }

    /// Optional directory containing YAML workflow manifests bundled with
    /// this plugin. Loaded by `WorkflowManifestLoader` when the runtime is
    /// built. Returning `None` means the plugin ships no workflows (the
    /// default).
    fn workflow_manifest_dir(&self) -> Option<PathBuf> {
        None
    }
}
