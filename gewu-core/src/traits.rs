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
    CapabilityDescriptor, ExperienceQuery, Message, ProviderEvent, RecalledExperience,
    SpecialistDescriptor, ToolContext, ToolExecutionPolicy, ToolResult,
};
use async_trait::async_trait;
use futures::stream::BoxStream;
use schemars::schema::RootSchema;
use serde_json::Value;
use sqlx::SqlitePool;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

/// Defines a tool that the agent can call during execution.
///
/// The `Tool` trait is the core extension point for GeWu. Implement this trait
/// to create custom tools that the agent can invoke via the LLM.
///
/// # Example
///
/// ```
/// use async_trait::async_trait;
/// use std::error::Error;
/// use gewu_core::traits::Tool;
/// use gewu_core::types::{ToolResult, ToolContext};
/// use schemars::schema::RootSchema;
/// use schemars::JsonSchema;
/// use serde::Deserialize;
///
/// #[derive(JsonSchema, Deserialize)]
/// struct MyParams {
///     input: String,
/// }
///
/// struct MyTool;
///
/// #[async_trait]
/// impl Tool for MyTool {
///     fn name(&self) -> &str { "my_tool" }
///     fn description(&self) -> &str { "A description of what my tool does" }
///     fn schema(&self) -> RootSchema { schemars::schema_for!(MyParams) }
///     async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
///         let params: MyParams = serde_json::from_value(args)?;
///         Ok(ToolResult::new("my_tool", format!("result: {}", params.input)))
///     }
/// }
/// ```
#[async_trait]
pub trait Tool: Send + Sync {
    /// Returns the unique name of this tool.
    ///
    /// This name is used by the LLM to request this tool and must be
    /// unique among all tools registered with the agent.
    fn name(&self) -> &str;

    /// Returns a description of what this tool does.
    ///
    /// The description is passed to the LLM so it can decide when to
    /// call this tool based on user requests.
    fn description(&self) -> &str;

    /// Returns the JSON schema for this tool's parameters.
    ///
    /// The schema is used by the LLM to generate valid arguments
    /// when calling this tool.
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

    /// Executes the tool with the given arguments.
    ///
    /// # Arguments
    ///
    /// * `args` - JSON value containing the tool arguments (validated against the schema)
    /// * `ctx` - Execution context containing session ID, message ID, and abort signal
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>>;
}

/// Provides access to a collection of tools for execution.
///
/// The `ToolRegistry` trait is implemented by types that manage multiple tools,
/// such as the `Agent` itself. This abstraction allows tools to be loaded
/// dynamically and executed through a unified interface.
#[async_trait]
pub trait ToolRegistry: Send + Sync {
    /// Executes a tool by name with the given arguments.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the tool to execute
    /// * `args` - JSON arguments to pass to the tool
    /// * `ctx` - Execution context
    async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>>;

    /// Returns a list of all available tool names.
    fn list_tools(&self) -> Vec<String>;

    fn execution_policy(&self, _name: &str) -> Option<ToolExecutionPolicy> {
        None
    }
}

/// Provides access to a language model for generating responses.
///
/// The `LlmProvider` trait abstracts over different LLM backends (OpenAI, Anthropic, etc.).
/// Implement this trait to add support for new LLM providers.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Initializes the provider (optional).
    ///
    /// Called once before the provider is first used. Override to perform
    /// setup such as validating credentials or checking API availability.
    async fn init(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>
    where
        Self: Sized,
    {
        Ok(())
    }

    /// Streams a response from the LLM.
    ///
    /// # Arguments
    ///
    /// * `messages` - The conversation history to send to the LLM
    /// * `tools` - Available tools the LLM can call
    ///
    /// # Returns
    ///
    /// A stream of provider events representing the LLM's response
    async fn stream_response<'a>(
        &'a self,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    >;

    /// Lists available models for this provider.
    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>>;
}

/// Stores and manages conversation history.
///
/// The `Memory` trait abstracts over different storage backends for
/// conversation history. Implement this trait to add custom storage
/// solutions such as databases or persistent storage.
#[async_trait]
pub trait Memory: Send + Sync {
    /// Initializes the memory store (optional).
    async fn init(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>
    where
        Self: Sized,
    {
        Ok(())
    }

    /// Returns a slice of all messages in the conversation history.
    fn messages(&self) -> &[Message];

    async fn add_message(&mut self, message: Message) -> Result<(), Box<dyn Error + Send + Sync>>;

    async fn clear(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>;

    /// Returns a mutable slice of all messages.
    fn messages_mut(&mut self) -> &mut [Message];

    async fn replace(&mut self, messages: Vec<Message>)
    -> Result<(), Box<dyn Error + Send + Sync>>;
}

/// In-memory implementation of `Memory` using a `Vec<Message>`.
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

/// Abstraction for storing and retrieving experiences.
///
/// The `Experiences` trait provides a simple interface for recording
/// and retrieving past experiences to inform future problem-solving.
///
/// When used with an [`Agent`] via
/// [`AgentBuilder::with_experiences`](crate::agent::AgentBuilder::with_experiences),
/// a `search_experiences` tool is automatically registered, allowing the LLM
/// to recall relevant past experiences during problem-solving.
///
/// [`Agent`]: crate::agent::Agent
#[async_trait]
pub trait Experiences: Send + Sync {
    /// Stores a new experience and returns its unique ID.
    async fn store(&self, content: &str) -> Result<String, Box<dyn Error + Send + Sync>>;

    /// Recalls experiences similar to the query, sorted by similarity (highest first).
    async fn recall(
        &self,
        query: &ExperienceQuery,
    ) -> Result<Vec<RecalledExperience>, Box<dyn Error + Send + Sync>>;
}
