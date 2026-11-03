use anyhow::{Result, anyhow};
use async_trait::async_trait;
use gewu_core::Agent;
use gewu_core::traits::{LlmProvider, RequestLog, Tool, ToolRegistry};
use gewu_core::types::{ToolContext, ToolResult};
use gewu_storage::SqliteMemory;
use gewu_tools::task::AgentFactory;
use gewu_tools::{
    ApplyPatchTool, BashTool, BatchTool, CodeSearchTool, EditFileTool, GlobTool, GrepTool,
    ListTool, ReadFileTool, SkillInfo, SkillTool, TaskTool, TodoItem, TodoReadTool, TodoWriteTool,
    WebFetchPolicy, WebFetchTool, WebSearchTool, WriteFileTool,
};
use serde_json::Value;
use sqlx::SqlitePool;
use std::error::Error;
use std::sync::{Arc, Mutex};

const RESERVED_ADDITIONAL_TOOL_NAMES: &[&str] = &[
    "apply_patch",
    "write_file",
    "edit_file",
    "task",
    "skill",
    "todowrite",
    "todoread",
    "batch",
];

pub struct AgentConfig {
    pub name: String,
    pub description: String,
}

pub fn list_available_agents() -> Vec<AgentConfig> {
    vec![
        AgentConfig {
            name: "build".to_string(),
            description: "The default agent. Executes tools based on configured permissions.".to_string(),
        },
        AgentConfig {
            name: "explore".to_string(),
            description: "Fast agent specialized for exploring codebases.".to_string(),
        },
        AgentConfig {
            name: "general".to_string(),
            description: "General-purpose agent for researching complex questions and executing multi-step tasks.".to_string(),
        },
    ]
}

/// Returns the names of the available agent profiles.
pub fn list_agent_names() -> Vec<String> {
    list_available_agents()
        .into_iter()
        .map(|a| a.name)
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

/// Creates sub-agents through the CLI registry for the [`TaskTool`].
struct SqliteAgentFactory {
    provider: Arc<dyn LlmProvider>,
    pool: SqlitePool,
    skills: Vec<SkillInfo>,
    web_fetch: Arc<WebFetchPolicy>,
}

#[async_trait]
impl AgentFactory for SqliteAgentFactory {
    async fn create(
        &self,
        subagent_type: &str,
        resume: Option<String>,
    ) -> Result<(Agent, String), Box<dyn Error + Send + Sync>> {
        let session_id = resume.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let (agent, _registry) = create_agent(
            subagent_type,
            self.provider.clone(),
            self.pool.clone(),
            session_id.clone(),
            Vec::new(),
            None,
            self.skills.clone(),
            self.web_fetch.clone(),
        )
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> { e.to_string().into() })?;
        Ok((agent, session_id))
    }
}

/// Builds the task tool wired to the CLI registry.
fn task_tool(
    provider: Arc<dyn LlmProvider>,
    pool: SqlitePool,
    skills: Vec<SkillInfo>,
    web_fetch: Arc<WebFetchPolicy>,
) -> TaskTool {
    let factory = Arc::new(SqliteAgentFactory {
        provider,
        pool,
        skills,
        web_fetch,
    });
    let agents = list_available_agents()
        .into_iter()
        .map(|a| (a.name, a.description))
        .collect();
    TaskTool::new(factory, agents)
}

fn build_tool_registry_and_list(
    name: &str,
    provider: Arc<dyn LlmProvider>,
    pool: SqlitePool,
    working_directory: std::path::PathBuf,
    additional_tools: Vec<Arc<dyn Tool>>,
    skills: Vec<SkillInfo>,
    web_fetch: Arc<WebFetchPolicy>,
) -> Result<(Arc<dyn ToolRegistry>, Vec<Arc<dyn Tool>>)> {
    let mut tools: Vec<Box<dyn Tool>> = match name {
        "build" => {
            let todos = Arc::new(Mutex::new(Vec::<TodoItem>::new()));
            vec![
                Box::new(task_tool(
                    provider.clone(),
                    pool.clone(),
                    skills.clone(),
                    web_fetch.clone(),
                )),
                Box::new(ReadFileTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(ApplyPatchTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(WriteFileTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(EditFileTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(GlobTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(BashTool {
                    working_directory: working_directory.clone(),
                    safe_mode: false,
                }),
                Box::new(GrepTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(WebFetchTool::with_shared_policy(web_fetch.clone())),
                Box::new(ListTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(CodeSearchTool),
                Box::new(WebSearchTool),
                Box::new(TodoWriteTool::new(todos.clone())),
                Box::new(TodoReadTool::new(todos.clone())),
            ]
        }
        "explore" => {
            vec![
                Box::new(ReadFileTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(GlobTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(BashTool {
                    working_directory: working_directory.clone(),
                    safe_mode: true,
                }),
                Box::new(GrepTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(WebFetchTool::with_shared_policy(web_fetch.clone())),
                Box::new(ListTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(CodeSearchTool),
                Box::new(WebSearchTool),
            ]
        }
        "general" => {
            let todos = Arc::new(Mutex::new(Vec::<TodoItem>::new()));
            vec![
                Box::new(task_tool(
                    provider.clone(),
                    pool.clone(),
                    skills.clone(),
                    web_fetch.clone(),
                )),
                Box::new(ReadFileTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(ApplyPatchTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(WriteFileTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(EditFileTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(GlobTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(BashTool {
                    working_directory: working_directory.clone(),
                    safe_mode: false,
                }),
                Box::new(GrepTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(WebFetchTool::with_shared_policy(web_fetch.clone())),
                Box::new(ListTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(CodeSearchTool),
                Box::new(WebSearchTool),
                Box::new(TodoWriteTool::new(todos.clone())),
                Box::new(TodoReadTool::new(todos.clone())),
            ]
        }
        _ => return Err(anyhow!("Unknown agent type: {}", name)),
    };

    if !skills.is_empty() {
        tools.push(Box::new(SkillTool::new(skills)));
    }

    let mut tools_arc: Vec<Arc<dyn Tool>> = tools
        .into_iter()
        .map(|t| Arc::from(t) as Arc<dyn Tool>)
        .collect();
    let mut names = tools_arc
        .iter()
        .map(|tool| tool.name().to_string())
        .collect::<std::collections::HashSet<_>>();
    for tool in &additional_tools {
        anyhow::ensure!(
            !RESERVED_ADDITIONAL_TOOL_NAMES.contains(&tool.name()),
            "Additional tool name '{}' is reserved",
            tool.name()
        );
        anyhow::ensure!(
            names.insert(tool.name().to_string()),
            "Duplicate tool name: {}",
            tool.name()
        );
    }
    tools_arc.extend(additional_tools);

    let registry: Arc<dyn ToolRegistry> = Arc::new(ToolRegistryImpl {
        tools: Arc::new(tools_arc.clone()),
    });

    Ok((registry, tools_arc))
}

pub async fn create_agent(
    name: &str,
    provider: Arc<dyn LlmProvider>,
    pool: SqlitePool,
    session_id: String,
    additional_tools: Vec<Arc<dyn Tool>>,
    log: Option<RequestLog>,
    skills: Vec<SkillInfo>,
    web_fetch: Arc<WebFetchPolicy>,
) -> Result<(Agent, Arc<dyn ToolRegistry>)> {
    let working_directory = std::env::current_dir()?;

    let memory = SqliteMemory::new(pool.clone(), session_id).await?;

    let (registry, tools_arc) = build_tool_registry_and_list(
        name,
        provider.clone(),
        pool,
        working_directory,
        additional_tools,
        skills.clone(),
        web_fetch,
    )?;

    let batch_tool: Arc<dyn Tool> = Arc::new(BatchTool::new(registry.clone()));
    let mut all_tools = vec![batch_tool];
    all_tools.extend(tools_arc);

    let mut system_prompt = match name {
        "build" => include_str!("prompts/build.txt"),
        "explore" => include_str!("prompts/explore.txt"),
        "general" => include_str!("prompts/general.txt"),
        _ => return Err(anyhow!("Unknown agent type: {}", name)),
    }
    .to_string();
    if !skills.is_empty() {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(&gewu_tools::skill::available_skills_section(&skills));
    }

    let mut builder = Agent::builder()
        .llm(provider)
        .with_tools(all_tools)
        .memory(Box::new(memory))
        .system_prompt(&system_prompt);
    if let Some(log) = log {
        builder = builder.with_log(log);
    }
    let agent = builder.build()?;

    Ok((agent, registry))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permissive() -> Arc<WebFetchPolicy> {
        Arc::new(WebFetchPolicy::default())
    }
    use futures::stream::BoxStream;
    use gewu_core::types::{Message, ProviderEvent};
    use std::error::Error;
    use tempfile::tempdir;

    struct DummyLlm;

    struct NamedTool(&'static str);

    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }

        fn description(&self) -> &str {
            "test tool"
        }

        fn schema(&self) -> schemars::schema::RootSchema {
            schemars::schema_for!(serde_json::Value)
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
            Ok(ToolResult::new(self.0, "ok"))
        }
    }

    #[async_trait]
    impl LlmProvider for DummyLlm {
        fn build(
            &self,
            _messages: &[Message],
            _tools: &[Arc<dyn Tool>],
        ) -> Result<Value, Box<dyn Error + Send + Sync>> {
            Ok(Value::Null)
        }

        async fn send<'a>(
            &'a self,
            _request: Value,
        ) -> Result<
            BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
            Box<dyn Error + Send + Sync>,
        > {
            Err("Not implemented".into())
        }

        async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn apply_patch_is_only_in_mutating_profiles() {
        let pool = SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        let directory = tempdir().unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);

        for profile in ["build", "general"] {
            let (_, tools) = build_tool_registry_and_list(
                profile,
                provider.clone(),
                pool.clone(),
                directory.path().to_path_buf(),
                Vec::new(),
                Vec::new(),
                permissive(),
            )
            .unwrap();
            assert!(tools.iter().any(|tool| tool.name() == "apply_patch"));
        }

        let (_, tools) = build_tool_registry_and_list(
            "explore",
            provider,
            pool,
            directory.path().to_path_buf(),
            Vec::new(),
            Vec::new(),
            permissive(),
        )
        .unwrap();
        assert!(!tools.iter().any(|tool| tool.name() == "apply_patch"));
    }

    #[tokio::test]
    async fn create_agent_accepts_request_log() {
        let pool = SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);
        let log: RequestLog = Arc::new(Mutex::new(Box::new(Vec::<u8>::new())));

        let (agent, _) = create_agent(
            "build",
            provider,
            pool,
            "session".to_string(),
            Vec::new(),
            Some(log),
            Vec::new(),
            permissive(),
        )
        .await
        .expect("create_agent should accept a request log");

        assert!(!agent.list_tools().is_empty());
    }

    #[tokio::test]
    async fn create_agent_registers_skill_tool_when_skills_exist() {
        let pool = SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);
        let directory = tempdir().unwrap();
        let skill_dir = directory.path().join("demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\nDemo body",
        )
        .unwrap();
        let skills = gewu_tools::skill::discover(&[directory.path().to_path_buf()]);

        let (agent, _) = create_agent(
            "build",
            provider,
            pool,
            "session".to_string(),
            Vec::new(),
            None,
            skills,
            permissive(),
        )
        .await
        .expect("create_agent should register the skill tool");

        assert!(agent.list_tools().iter().any(|name| name == "skill"));
    }

    #[tokio::test]
    async fn additional_tools_cannot_inject_or_duplicate_builtin_names() {
        let pool = SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        let directory = tempdir().unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);

        let reserved = build_tool_registry_and_list(
            "explore",
            provider.clone(),
            pool.clone(),
            directory.path().to_path_buf(),
            vec![Arc::new(NamedTool("apply_patch"))],
            Vec::new(),
            permissive(),
        );
        assert!(reserved.err().unwrap().to_string().contains("reserved"));

        let batch = build_tool_registry_and_list(
            "explore",
            provider.clone(),
            pool.clone(),
            directory.path().to_path_buf(),
            vec![Arc::new(NamedTool("batch"))],
            Vec::new(),
            permissive(),
        );
        assert!(batch.err().unwrap().to_string().contains("reserved"));

        let duplicate = build_tool_registry_and_list(
            "build",
            provider,
            pool,
            directory.path().to_path_buf(),
            vec![Arc::new(NamedTool("websearch"))],
            Vec::new(),
            permissive(),
        );
        assert!(
            duplicate
                .err()
                .unwrap()
                .to_string()
                .contains("Duplicate tool name")
        );
    }
}
