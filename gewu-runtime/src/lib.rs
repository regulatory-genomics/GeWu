pub mod capability;
pub mod command_tool;
pub mod command_tool_registry;
pub mod composer;
pub mod factory;
pub mod execution_trace;
pub mod hooks;
pub mod manifest;
pub mod plugin_host;
pub mod providers;
pub mod registry;
pub mod runtime;
pub mod skill_registry;
pub mod sop_registry;
pub mod tools;
pub mod trace_diff;
pub mod workflow;
pub mod workflow_registry;
pub mod workflow_tools;

pub use capability::{CapabilityRegistry, ToolFactoryProvider};
pub use command_tool::{CommandTool, CommandToolDef};
pub use command_tool_registry::{CommandToolProvider, CommandToolRegistry};
pub use composer::{
    COMPOSER_CAPABILITIES, ComposerContext, ComposerToolFactory, RegisterToolTool,
    WriteSpecialistTool, composer_specialist_descriptor,
};
pub use factory::RuntimeFactory;
pub use execution_trace::{NoopTraceSink, SqliteTraceSink, StartTraceRun, TraceContext, TraceSink};
pub use hooks::{HookRegistry, HttpHookProvider};
pub use manifest::loader::SpecialistManifestLoader;
pub use manifest::runtime_plugin_loader::RuntimePluginManifestLoader;
pub use manifest::schema::{PromptSource, SpecialistManifest};
pub use manifest::skill::{SkillFrontmatter, SkillManifest};
pub use manifest::skill_loader::SkillManifestLoader;
pub use manifest::workflow::{
    ConditionalEdgeSpec, EdgeSpec, InlineSpecialist, NodeSpec, SpecialistRef, WorkflowManifest,
};
pub use manifest::workflow_loader::WorkflowManifestLoader;
pub use plugin_host::{PluginConstructor, PluginHost, PluginRegistry};
pub use registry::{
    AgentConfig, BuiltinToolFactory, create_agent, find_descriptor, list_available_agents,
    resolve_tools, specialist_descriptors,
};
pub use runtime::{GeWuRuntime, GeWuRuntimeBuilder, SpecialistExecutionEvent};
pub use skill_registry::SkillRegistry;
pub use sop_registry::{SopDescriptor, SopMetadata, SopRegistry};
pub use tools::TaskTool;
pub use trace_diff::{DiffEntry, DiffKind, DiffOptions, diff_blackboards, format_diff};
pub use workflow::{Predicate, WorkflowCompiler, validate as validate_workflow, validate_with_ids};
pub use workflow_registry::WorkflowRegistry;
