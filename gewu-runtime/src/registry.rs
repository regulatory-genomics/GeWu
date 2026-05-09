use crate::command_tool_registry::CommandToolRegistry;
use crate::composer::{RegisterToolTool, WriteSpecialistTool};
use crate::tools::{GetTaskStatusTool, ListChildTasksTool, TaskTool};
use crate::workflow_tools::{ComposeWorkflowTool, MemoryDayQueryTool, RunWorkflowTool};
use anyhow::{Result, anyhow};
use gewu_core::traits::{AssemblyContext, LlmProvider, Tool, ToolFactory, ToolRegistry};
use gewu_core::types::SpecialistDescriptor;
use gewu_tools::{
    BashTool, CodeSearchTool, EditFileTool, GlobTool, GrepTool, ListTool, ReadFileTool,
    ReportResultTool, TodoItem, TodoReadTool, TodoWriteTool, WebFetchTool, WebSearchTool,
    WriteFileTool,
};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct AgentConfig {
    pub name: String,
    pub description: String,
}

pub fn specialist_descriptors() -> Vec<SpecialistDescriptor> {
    vec![
        SpecialistDescriptor {
            id: "build".into(),
            description: "The default agent. Executes tools based on configured permissions.".into(),
            capabilities: vec![
                "task", "read_file", "write_file", "edit_file", "glob",
                "bash", "grep", "web_fetch", "list", "code_search",
                "web_search", "todo", "report_result", "get_task_status", "list_child_tasks",
                "compose_workflow", "run_workflow", "register_tool", "write_specialist",
                "memory_day_query",
            ].into_iter().map(String::from).collect(),
            skills: Vec::new(),
            system_prompt: include_str!("prompts/build.txt").into(),
        },
        SpecialistDescriptor {
            id: "explore".into(),
            description: "Fast agent specialized for exploring codebases.".into(),
            capabilities: vec![
                "read_file", "glob", "bash_safe", "grep", "web_fetch",
                "list", "code_search", "web_search", "report_result",
            ].into_iter().map(String::from).collect(),
            skills: Vec::new(),
            system_prompt: include_str!("prompts/explore.txt").into(),
        },
        SpecialistDescriptor {
            id: "general".into(),
            description: "General-purpose agent for researching complex questions and executing multi-step tasks.".into(),
            capabilities: vec![
                "task", "read_file", "write_file", "edit_file", "glob",
                "bash", "grep", "web_fetch", "list", "code_search",
                "web_search", "todo", "report_result", "get_task_status", "list_child_tasks",
                "compose_workflow", "run_workflow", "register_tool", "write_specialist",
                "memory_day_query",
            ].into_iter().map(String::from).collect(),
            skills: Vec::new(),
            system_prompt: include_str!("prompts/general.txt").into(),
        },
    ]
}

pub fn list_available_agents() -> Vec<AgentConfig> {
    specialist_descriptors()
        .iter()
        .map(|d| AgentConfig {
            name: d.id.clone(),
            description: d.description.clone(),
        })
        .collect()
}

/// Look up a specialist descriptor by id.
pub fn find_descriptor(name: &str) -> Option<SpecialistDescriptor> {
    specialist_descriptors().into_iter().find(|d| d.id == name)
}

pub struct BuiltinToolFactory;

impl ToolFactory for BuiltinToolFactory {
    fn capabilities(&self) -> Vec<String> {
        vec![
            "read_file",
            "write_file",
            "edit_file",
            "glob",
            "bash",
            "bash_safe",
            "grep",
            "web_fetch",
            "web_search",
            "list",
            "code_search",
            "todo",
            "task",
            "get_task_status",
            "list_child_tasks",
            "report_result",
            "compose_workflow",
            "run_workflow",
            "memory_day_query",
            "register_tool",
            "write_specialist",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    fn create_tools(
        &self,
        capability: &str,
        ctx: &AssemblyContext,
    ) -> std::result::Result<Vec<Box<dyn Tool>>, Box<dyn std::error::Error + Send + Sync>> {
        let wd = ctx.working_directory.clone();
        match capability {
            "read_file" => Ok(vec![Box::new(ReadFileTool {
                working_directory: wd,
            })]),
            "write_file" => Ok(vec![Box::new(WriteFileTool {
                working_directory: wd,
            })]),
            "edit_file" => Ok(vec![Box::new(EditFileTool {
                working_directory: wd,
            })]),
            "glob" => Ok(vec![Box::new(GlobTool {
                working_directory: wd,
            })]),
            "bash" => Ok(vec![Box::new(BashTool {
                working_directory: wd,
                safe_mode: false,
            })]),
            "bash_safe" => Ok(vec![Box::new(BashTool {
                working_directory: wd,
                safe_mode: true,
            })]),
            "grep" => Ok(vec![Box::new(GrepTool {
                working_directory: wd,
            })]),
            "web_fetch" => Ok(vec![Box::new(WebFetchTool)]),
            "web_search" => Ok(vec![Box::new(WebSearchTool)]),
            "list" => Ok(vec![Box::new(ListTool {
                working_directory: wd,
            })]),
            "code_search" => Ok(vec![Box::new(CodeSearchTool)]),
            "todo" => {
                let todos = Arc::new(Mutex::new(Vec::<TodoItem>::new()));
                Ok(vec![
                    Box::new(TodoWriteTool::new(todos.clone())),
                    Box::new(TodoReadTool::new(todos)),
                ])
            }
            "task" => Ok(vec![Box::new(TaskTool::new(
                ctx.provider.clone(),
                ctx.pool.clone(),
            ))]),
            "get_task_status" => Ok(vec![Box::new(GetTaskStatusTool {
                pool: ctx.pool.clone(),
            })]),
            "list_child_tasks" => Ok(vec![Box::new(ListChildTasksTool {
                pool: ctx.pool.clone(),
            })]),
            "compose_workflow" => Ok(vec![Box::new(ComposeWorkflowTool::new(
                ctx.provider.clone(),
                ctx.pool.clone(),
            ))]),
            "memory_day_query" => Ok(vec![Box::new(MemoryDayQueryTool)]),
            "run_workflow" => Ok(vec![Box::new(
                RunWorkflowTool::new(
                    ctx.provider.clone(),
                    ctx.pool.clone(),
                    metadata_dirs(&ctx.metadata_json, "workflow_dirs"),
                )
                .with_runtime_plugin_dirs(metadata_dirs(&ctx.metadata_json, "runtime_plugin_dirs"))
                .with_skill_dirs(metadata_dirs(&ctx.metadata_json, "skill_dirs")),
            )]),
            "register_tool" => Ok(vec![Box::new(RegisterToolTool::new(
                CommandToolRegistry::default_dir(),
            ))]),
            "write_specialist" => Ok(vec![Box::new(WriteSpecialistTool::new(
                crate::composer::default_specialists_dir(),
            ))]),
            "report_result" => Ok(vec![Box::new(ReportResultTool)]),
            _ => Err(format!("Unknown capability: {capability}").into()),
        }
    }
}

fn metadata_dirs(metadata: &serde_json::Value, key: &str) -> Vec<PathBuf> {
    metadata
        .get(key)
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

pub fn resolve_tools(
    capabilities: &[String],
    working_directory: &Path,
    provider: Arc<dyn LlmProvider>,
    pool: SqlitePool,
) -> Vec<Box<dyn Tool>> {
    let runtime = crate::runtime::GeWuRuntime::builder()
        .provider(provider)
        .pool(pool)
        .register_builtin_capabilities()
        .register_builtin_specialists()
        .build();

    match runtime {
        Ok(runtime) => runtime
            .resolve_capabilities_in_directory(capabilities, working_directory)
            .unwrap_or_else(|e| {
                tracing::warn!("Failed to resolve capabilities: {}", e);
                vec![]
            }),
        Err(e) => {
            tracing::warn!("Failed to build runtime for resolve_tools: {}", e);
            vec![]
        }
    }
}

pub async fn create_agent(
    name: &str,
    provider: Arc<dyn LlmProvider>,
    pool: SqlitePool,
    session_id: String,
    additional_tools: Vec<Arc<dyn Tool>>,
) -> Result<(gewu_core::Agent, Arc<dyn ToolRegistry>)> {
    let runtime = crate::runtime::GeWuRuntime::builder()
        .provider(provider)
        .pool(pool)
        .register_builtin_capabilities()
        .register_builtin_specialists()
        .build()
        .map_err(|e| anyhow!("Failed to build runtime: {e}"))?;

    runtime
        .build_specialist(name, session_id, additional_tools)
        .await
}

#[cfg(test)]
mod tests {
    use super::{BuiltinToolFactory, specialist_descriptors};
    use gewu_core::traits::ToolFactory;

    #[test]
    fn builtin_factory_exposes_task_status_tools() {
        let capabilities = BuiltinToolFactory.capabilities();
        assert!(capabilities.contains(&"get_task_status".to_string()));
        assert!(capabilities.contains(&"list_child_tasks".to_string()));
    }

    #[test]
    fn primary_agents_can_query_child_task_status() {
        let descriptors = specialist_descriptors();
        for agent_id in ["build", "general"] {
            let descriptor = descriptors
                .iter()
                .find(|descriptor| descriptor.id == agent_id)
                .expect("primary agent descriptor exists");
            assert!(
                descriptor
                    .capabilities
                    .contains(&"get_task_status".to_string())
            );
            assert!(
                descriptor
                    .capabilities
                    .contains(&"list_child_tasks".to_string())
            );
        }
    }
}
