pub mod capability;
pub mod factory;
pub mod manifest;
pub mod plugin_host;
pub mod providers;
pub mod registry;
pub mod runtime;
pub mod tools;

pub use capability::{CapabilityRegistry, ToolFactoryProvider};
pub use factory::RuntimeFactory;
pub use manifest::loader::SpecialistManifestLoader;
pub use manifest::schema::{PromptSource, SpecialistManifest};
pub use plugin_host::{PluginConstructor, PluginHost, PluginRegistry};
pub use registry::{
    AgentConfig, BuiltinToolFactory, create_agent, find_descriptor, list_available_agents,
    resolve_tools, specialist_descriptors,
};
pub use runtime::{GeWuRuntime, GeWuRuntimeBuilder};
pub use tools::TaskTool;
