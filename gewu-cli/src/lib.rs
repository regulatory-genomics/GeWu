pub mod config;
pub mod mcp_loader;
pub mod registry;
pub mod tools;

pub use config::LoadedConfig;
pub use registry::{
    create_agent, find_descriptor, list_available_agents, resolve_tools,
    specialist_descriptors, AgentConfig,
};
