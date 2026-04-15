use crate::tools::TaskTool;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use gewu_core::Agent;
use gewu_core::traits::{LlmProvider, Tool, ToolRegistry};
use gewu_core::types::{SpecialistDescriptor, ToolCapability, ToolContext, ToolResult};
use gewu_storage::SqliteMemory;
use gewu_tools::{
    BashTool, BatchTool, CodeSearchTool, EditFileTool, GlobTool, GrepTool, ListTool, ReadFileTool,
    ReportResultTool, TodoItem, TodoReadTool, TodoWriteTool, WebFetchTool, WebSearchTool,
    WriteFileTool,
};
use serde_json::Value;
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub struct AgentConfig {
    pub name: String,
    pub description: String,
}

/// All known specialist descriptors.
pub fn specialist_descriptors() -> Vec<SpecialistDescriptor> {
    vec![
        SpecialistDescriptor {
            id: "build",
            description: "The default agent. Executes tools based on configured permissions.",
            capabilities: vec![
                ToolCapability::Task,
                ToolCapability::ReadFile,
                ToolCapability::WriteFile,
                ToolCapability::EditFile,
                ToolCapability::Glob,
                ToolCapability::Bash { safe_mode: false },
                ToolCapability::Grep,
                ToolCapability::WebFetch,
                ToolCapability::List,
                ToolCapability::CodeSearch,
                ToolCapability::WebSearch,
                ToolCapability::Todo,
                ToolCapability::ReportResult,
            ],
            system_prompt: include_str!("prompts/build.txt"),
        },
        SpecialistDescriptor {
            id: "explore",
            description: "Fast agent specialized for exploring codebases.",
            capabilities: vec![
                ToolCapability::ReadFile,
                ToolCapability::Glob,
                ToolCapability::Bash { safe_mode: true },
                ToolCapability::Grep,
                ToolCapability::WebFetch,
                ToolCapability::List,
                ToolCapability::CodeSearch,
                ToolCapability::WebSearch,
                ToolCapability::ReportResult,
            ],
            system_prompt: include_str!("prompts/explore.txt"),
        },
        SpecialistDescriptor {
            id: "general",
            description: "General-purpose agent for researching complex questions and executing multi-step tasks.",
            capabilities: vec![
                ToolCapability::Task,
                ToolCapability::ReadFile,
                ToolCapability::WriteFile,
                ToolCapability::EditFile,
                ToolCapability::Glob,
                ToolCapability::Bash { safe_mode: false },
                ToolCapability::Grep,
                ToolCapability::WebFetch,
                ToolCapability::List,
                ToolCapability::CodeSearch,
                ToolCapability::WebSearch,
                ToolCapability::Todo,
                ToolCapability::ReportResult,
            ],
            system_prompt: include_str!("prompts/general.txt"),
        },
    ]
}

pub fn list_available_agents() -> Vec<AgentConfig> {
    specialist_descriptors()
        .iter()
        .map(|d| AgentConfig {
            name: d.id.to_string(),
            description: d.description.to_string(),
        })
        .collect()
}

/// Look up a specialist descriptor by id.
pub fn find_descriptor(name: &str) -> Option<SpecialistDescriptor> {
    specialist_descriptors().into_iter().find(|d| d.id == name)
}

/// Resolve a list of capabilities into concrete tool instances.
pub fn resolve_tools(
    capabilities: &[ToolCapability],
    working_directory: &PathBuf,
    provider: Arc<dyn LlmProvider>,
    pool: SqlitePool,
    todos: Arc<Mutex<Vec<TodoItem>>>,
) -> Vec<Box<dyn Tool>> {
    capabilities
        .iter()
        .flat_map(|cap| -> Vec<Box<dyn Tool>> {
            match cap {
                ToolCapability::ReadFile => vec![Box::new(ReadFileTool {
                    working_directory: working_directory.clone(),
                })],
                ToolCapability::WriteFile => vec![Box::new(WriteFileTool {
                    working_directory: working_directory.clone(),
                })],
                ToolCapability::EditFile => vec![Box::new(EditFileTool {
                    working_directory: working_directory.clone(),
                })],
                ToolCapability::Glob => vec![Box::new(GlobTool {
                    working_directory: working_directory.clone(),
                })],
                ToolCapability::Bash { safe_mode } => vec![Box::new(BashTool {
                    working_directory: working_directory.clone(),
                    safe_mode: *safe_mode,
                })],
                ToolCapability::Grep => vec![Box::new(GrepTool {
                    working_directory: working_directory.clone(),
                })],
                ToolCapability::WebFetch => vec![Box::new(WebFetchTool)],
                ToolCapability::WebSearch => vec![Box::new(WebSearchTool)],
                ToolCapability::List => vec![Box::new(ListTool {
                    working_directory: working_directory.clone(),
                })],
                ToolCapability::CodeSearch => vec![Box::new(CodeSearchTool)],
                ToolCapability::Todo => vec![
                    Box::new(TodoWriteTool::new(todos.clone())),
                    Box::new(TodoReadTool::new(todos.clone())),
                ],
                ToolCapability::Task => {
                    vec![Box::new(TaskTool::new(provider.clone(), pool.clone()))]
                }
                ToolCapability::ReportResult => {
                    vec![Box::new(ReportResultTool)]
                }
            }
        })
        .collect()
}

struct ToolRegistryImpl {
    tools: Arc<Vec<Arc<dyn Tool>>>,
}

#[async_trait]
impl ToolRegistry for ToolRegistryImpl {
    async fn execute(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let tool = self.tools.iter().find(|t| t.name() == name);
        match tool {
            Some(t) => t.execute(args, ctx).await,
            None => Err(format!("Tool not found: {}", name).into()),
        }
    }

    fn list_tools(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }
}

pub async fn create_agent(
    name: &str,
    provider: Arc<dyn LlmProvider>,
    pool: SqlitePool,
    session_id: String,
    additional_tools: Vec<Arc<dyn Tool>>,
) -> Result<(Agent, Arc<dyn ToolRegistry>)> {
    let descriptor = find_descriptor(name)
        .ok_or_else(|| anyhow!("Unknown agent type: {}", name))?;

    let working_directory = std::env::current_dir()?;
    let memory = SqliteMemory::new(pool.clone(), session_id).await?;
    let todos = Arc::new(Mutex::new(Vec::<TodoItem>::new()));

    let tools = resolve_tools(
        &descriptor.capabilities,
        &working_directory,
        provider.clone(),
        pool.clone(),
        todos,
    );

    let mut tools_arc: Vec<Arc<dyn Tool>> = tools
        .into_iter()
        .map(|t| Arc::from(t) as Arc<dyn Tool>)
        .collect();
    tools_arc.extend(additional_tools);

    let tools_for_registry = Arc::new(tools_arc.clone());
    let registry: Arc<dyn ToolRegistry> = Arc::new(ToolRegistryImpl {
        tools: tools_for_registry,
    });

    let batch_tool: Arc<dyn Tool> = Arc::new(BatchTool::new(registry.clone()));
    let mut all_tools = vec![batch_tool];
    all_tools.extend(tools_arc);

    let agent = Agent::builder()
        .llm(provider)
        .with_tools(all_tools)
        .memory(Box::new(memory))
        .system_prompt(descriptor.system_prompt)
        .build()?;

    Ok((agent, registry))
}
