pub mod capability;
pub mod command_tool;
pub mod command_tool_registry;
pub mod composer;
pub mod factory;
pub mod manifest;
pub mod plugin_host;
pub mod providers;
pub mod registry;
pub mod runtime;
pub mod tools;
pub mod trace_diff;
pub mod workflow;
pub mod workflow_registry;
pub mod workflow_tools;

pub use capability::{CapabilityRegistry, ToolFactoryProvider};
pub use command_tool::{CommandTool, CommandToolDef};
pub use command_tool_registry::{CommandToolProvider, CommandToolRegistry};
pub use factory::RuntimeFactory;
pub use manifest::loader::SpecialistManifestLoader;
pub use manifest::schema::{PromptSource, SpecialistManifest};
pub use manifest::workflow::{
    ConditionalEdgeSpec, EdgeSpec, NodeSpec, WorkflowManifest,
};
pub use manifest::workflow_loader::WorkflowManifestLoader;
pub use plugin_host::{PluginConstructor, PluginHost, PluginRegistry};
pub use registry::{
    AgentConfig, BuiltinToolFactory, create_agent, find_descriptor, list_available_agents,
    resolve_tools, specialist_descriptors,
};
pub use runtime::{GeWuRuntime, GeWuRuntimeBuilder};
pub use tools::TaskTool;
pub use trace_diff::{DiffEntry, DiffKind, DiffOptions, diff_blackboards, format_diff};
pub use workflow::{Predicate, WorkflowCompiler, validate as validate_workflow, validate_with_ids};
pub use workflow_registry::WorkflowRegistry;
pub use composer::{
    COMPOSER_CAPABILITIES, ComposerContext, ComposerToolFactory, RegisterToolTool,
    composer_specialist_descriptor,
};
