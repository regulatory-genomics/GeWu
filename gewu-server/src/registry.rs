use anyhow::{Result, anyhow};
use async_trait::async_trait;
use gewu_core::traits::{LlmProvider, RequestLog, Tool, ToolRegistry};
use gewu_core::types::{PathEnforcement, SessionState, ToolContext, ToolResult};
use gewu_core::{Agent, AgentTemplate};
use gewu_storage::{SqliteMemory, SqliteSessionStateStore};
use gewu_tools::task::{AgentFactory, AgentOverrides};
use gewu_tools::workflow::{
    WorkflowCancelTool, WorkflowManager, WorkflowStartTool, WorkflowStatusTool, WorkflowWaitTool,
};
use gewu_tools::{
    ApplyPatchTool, BashTool, CodeSearchTool, EditFileTool, GlobTool, GrepTool, ListTool,
    ReadFileTool, SkillInfo, SkillTool, TaskTool, TodoReadTool, TodoWriteTool, WebFetchTool,
    WebSearchTool, WriteFileTool,
};
use serde_json::Value;
use sqlx::SqlitePool;
use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::sync::{Arc, Mutex};

use crate::config::AgentEntry;

const RESERVED_ADDITIONAL_TOOL_NAMES: &[&str] = &[
    "apply_patch",
    "write_file",
    "edit_file",
    "task",
    "skill",
    "todowrite",
    "todoread",
    "workflow_start",
    "workflow_status",
    "workflow_wait",
    "workflow_cancel",
];

pub struct AgentConfig {
    pub name: String,
    pub description: String,
}

/// Everything the profile builders need to assemble one agent.
///
/// Grouped to keep the constructor signatures manageable; consumed by
/// [`create_agent`], [`agent_profile`], and [`build_tool_registry_and_list`].
pub struct ProfileInputs<'a> {
    pub model_catalog: Option<ModelCatalog>,
    pub name: &'a str,
    pub provider: Arc<dyn LlmProvider>,
    pub pool: SqlitePool,
    pub working_directory: std::path::PathBuf,
    pub additional_tools: Vec<Arc<dyn Tool>>,
    pub skills: Vec<SkillInfo>,
    pub web_fetch: Arc<gewu_tools::WebFetchPolicy>,
    pub enforcement: PathEnforcement,
    pub context_limit: Option<usize>,
    /// User-defined agents (from the config file) resolvable as subagent
    /// profiles alongside the built-ins.
    pub agents: Arc<BTreeMap<String, AgentEntry>>,
    pub session_id: &'a str,
}

#[derive(Clone)]
pub struct ModelCatalog {
    pub entries: Arc<std::collections::BTreeMap<String, crate::bootstrap::ModelEntry>>,
    pub default: String,
}

/// Tool registry plus the flattened tool list reported to clients.
type RegistryBundle = (Arc<dyn ToolRegistry>, Vec<Arc<dyn Tool>>);
/// Assembled tools, the profile's system prompt, and its tool registry.
type ProfileBundle = (Vec<Arc<dyn Tool>>, String, Arc<dyn ToolRegistry>);

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
            name: "plan".to_string(),
            description: "Read-only planning agent. Switch to build to implement changes.".to_string(),
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
///
/// Per-profile templates (tools, prompt, limits) are built once and cached;
/// spawning only supplies the per-session pieces. The parent conversation
/// arrives per call (from the task tool's [`ToolContext`]) so subagent
/// lineage is recorded without the factory holding any session state.
struct SqliteAgentFactory {
    model_catalog: Option<ModelCatalog>,
    provider: Arc<dyn LlmProvider>,
    pool: SqlitePool,
    skills: Vec<SkillInfo>,
    working_directory: std::path::PathBuf,
    web_fetch: Arc<gewu_tools::WebFetchPolicy>,
    enforcement: PathEnforcement,
    /// Model context window forwarded to subagent agents for compaction.
    context_limit: Option<usize>,
    /// User-defined agents resolvable as subagent profiles.
    agents: Arc<BTreeMap<String, AgentEntry>>,
    /// Cached per-profile templates: the expensive tool-registry/prompt
    /// build runs once per profile, spawns clone it.
    templates: Mutex<HashMap<String, AgentTemplate>>,
}

/// How deep `task` calls may nest: a top-level session may spawn subagents,
/// but a subagent cannot spawn further ones.
const MAX_SUBAGENT_DEPTH: usize = 1;

/// Tools a dynamically composed (workflow) agent must never receive, so
/// delegation stays one level deep even when its base profile has them.
const COMPOSED_AGENT_FORBIDDEN_TOOLS: &[&str] = &[
    "task",
    "workflow_start",
    "workflow_status",
    "workflow_wait",
    "workflow_cancel",
];

#[async_trait]
impl AgentFactory for SqliteAgentFactory {
    async fn create(
        &self,
        subagent_type: &str,
        resume: Option<String>,
        parent_session: &str,
    ) -> Result<(Agent, String), Box<dyn Error + Send + Sync>> {
        self.spawn_agent(subagent_type, resume, parent_session, Ok)
            .await
    }

    async fn create_with(
        &self,
        base: &str,
        overrides: &AgentOverrides,
        parent_session: &str,
    ) -> Result<(Agent, String), Box<dyn Error + Send + Sync>> {
        self.spawn_agent(base, None, parent_session, |template| {
            apply_overrides(base, template, overrides)
        })
        .await
    }

    async fn validate_overrides(
        &self,
        base: &str,
        overrides: &AgentOverrides,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let template = self
            .template_for(base)
            .map_err(|e| -> Box<dyn Error + Send + Sync> { e.to_string().into() })?;
        apply_overrides(base, template, overrides)
            .map(|_| ())
            .map_err(|e| -> Box<dyn Error + Send + Sync> { e.to_string().into() })
    }
}

/// Applies [`AgentOverrides`] to a base template: strips delegation tools,
/// narrows the tool allowlist (base tools only), and appends instructions.
fn apply_overrides(
    base: &str,
    template: AgentTemplate,
    overrides: &AgentOverrides,
) -> Result<AgentTemplate> {
    let base_tools: Vec<Arc<dyn Tool>> = template
        .tools()
        .iter()
        .filter(|tool| !COMPOSED_AGENT_FORBIDDEN_TOOLS.contains(&tool.name()))
        .cloned()
        .collect();
    let mut template = template.with_tools(base_tools);

    if let Some(allowlist) = &overrides.tools {
        let known: std::collections::HashSet<_> =
            template.tools().iter().map(|tool| tool.name()).collect();
        for name in allowlist {
            anyhow::ensure!(
                known.contains(&name.as_str()),
                "base profile '{}' has no tool '{}' (workflow agent tool lists narrow the base profile)",
                base,
                name
            );
        }
        let tools: Vec<Arc<dyn Tool>> = template
            .tools()
            .iter()
            .filter(|tool| allowlist.iter().any(|name| name == tool.name()))
            .cloned()
            .collect();
        template = template.with_tools(tools);
    }

    if let Some(instructions) = &overrides.instructions {
        let prompt = format!("{}\n\n{}", template.system_prompt(), instructions);
        template = template.with_system_prompt(prompt);
    }
    Ok(template)
}

impl SqliteAgentFactory {
    /// Shared spawn path for `create` and `create_with`: depth check,
    /// model resolution, session bookkeeping, reminder restore, then
    /// spawning the profile template with `adjust` applied to it.
    async fn spawn_agent(
        &self,
        profile: &str,
        resume: Option<String>,
        parent_session: &str,
        adjust: impl FnOnce(AgentTemplate) -> Result<AgentTemplate>,
    ) -> Result<(Agent, String), Box<dyn Error + Send + Sync>> {
        let depth = gewu_storage::subagent_depth(&self.pool, parent_session).await?;
        if depth >= MAX_SUBAGENT_DEPTH {
            return Err(format!(
                "subagent depth limit reached ({MAX_SUBAGENT_DEPTH}); subagents cannot launch further subagents"
            )
            .into());
        }
        let session_id = resume.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let (provider, context_limit) = if let Some(catalog) = &self.model_catalog {
            let selector = match gewu_storage::session_model(&self.pool, &session_id).await? {
                Some(selected) => selected,
                None => gewu_storage::session_model(&self.pool, parent_session)
                    .await?
                    .unwrap_or_else(|| catalog.default.clone()),
            };
            let entry = catalog
                .entries
                .get(&selector)
                .ok_or_else(|| format!("unknown subagent model: {selector}"))?;
            gewu_storage::set_session_model(&self.pool, &session_id, &selector).await?;
            (entry.provider.clone(), Some(entry.context_limit))
        } else {
            (self.provider.clone(), self.context_limit)
        };
        gewu_storage::record_subagent_session(&self.pool, &session_id, parent_session, profile)
            .await?;
        let template = self
            .template_for(profile)
            .map_err(|e| -> Box<dyn Error + Send + Sync> { e.to_string().into() })?;
        let template = adjust(template)
            .map_err(|e| -> Box<dyn Error + Send + Sync> { e.to_string().into() })?;
        let memory = SqliteMemory::new(self.pool.clone(), session_id.clone()).await?;
        // Restore the durable reminder state; a session without one (fresh,
        // or created before reminders were persisted) starts from the
        // profile default. The initial state is written immediately so it
        // is durable from the first turn.
        let reminder = match gewu_storage::get_reminder_opt(&self.pool, &session_id).await? {
            Some(stored) => stored,
            None => {
                let initial = if profile == "plan" {
                    profile_reminder(None, profile)
                } else {
                    gewu_core::agent::Reminder::default()
                };
                gewu_storage::set_reminder(&self.pool, &session_id, &initial).await?;
                initial
            }
        };
        let template = match context_limit {
            Some(limit) => template.with_context_limit(limit),
            None => template,
        };
        let agent =
            template
                .with_llm(provider)
                .spawn(session_id.as_str(), Box::new(memory), reminder);
        Ok((agent, session_id))
    }

    /// Returns a cached [`AgentTemplate`] for `profile`, building it on
    /// first use through the regular profile pipeline.
    ///
    /// Templates bake in the pool-backed session state, so spawned
    /// subagents persist their tool state (todo lists, ...) in SQLite
    /// under their session id. The LLM and context limit stay per-spawn:
    /// model resolution happens per `create` call.
    fn template_for(&self, profile: &str) -> Result<AgentTemplate> {
        if let Some(template) = self
            .templates
            .lock()
            .map_err(|_| anyhow!("agent template cache is poisoned"))?
            .get(profile)
        {
            return Ok(template.clone());
        }
        let working_directory = self.working_directory.clone();
        let (tools, system_prompt, _registry) = agent_profile(ProfileInputs {
            model_catalog: self.model_catalog.clone(),
            name: profile,
            provider: self.provider.clone(),
            pool: self.pool.clone(),
            working_directory: working_directory.clone(),
            additional_tools: Vec::new(),
            skills: self.skills.clone(),
            web_fetch: self.web_fetch.clone(),
            enforcement: self.enforcement,
            context_limit: self.context_limit,
            agents: self.agents.clone(),
            session_id: "",
        })?;
        let session_state =
            SessionState::new(Arc::new(SqliteSessionStateStore::new(self.pool.clone())));
        let template = Agent::builder()
            .llm(self.provider.clone())
            .with_tools(tools)
            .system_prompt(&system_prompt)
            .working_directory(working_directory)
            .with_enforcement(self.enforcement)
            .with_session_state(session_state)
            .build()?
            .template();
        self.templates
            .lock()
            .map_err(|_| anyhow!("agent template cache is poisoned"))?
            .insert(profile.to_string(), template.clone());
        Ok(template)
    }
}

/// Builds the agent factory shared by the task tool and the workflow
/// manager: both spawn subagents through the same registry pipeline.
#[allow(clippy::too_many_arguments)]
fn agent_factory(
    model_catalog: Option<ModelCatalog>,
    provider: Arc<dyn LlmProvider>,
    pool: SqlitePool,
    skills: Vec<SkillInfo>,
    working_directory: std::path::PathBuf,
    web_fetch: Arc<gewu_tools::WebFetchPolicy>,
    enforcement: PathEnforcement,
    context_limit: Option<usize>,
    agents: Arc<BTreeMap<String, AgentEntry>>,
) -> Arc<SqliteAgentFactory> {
    Arc::new(SqliteAgentFactory {
        model_catalog,
        provider,
        pool,
        skills,
        working_directory,
        web_fetch,
        enforcement,
        context_limit,
        agents: agents.clone(),
        templates: Mutex::new(HashMap::new()),
    })
}

/// Builds the task tool wired to the CLI registry. `allowed` filters which
/// subagent types are advertised and permitted; empty means all of them.
/// Config-defined agents are advertised alongside the built-ins (still
/// subject to `allowed`, so restricted profiles like plan never see them).
fn task_tool(
    factory: Arc<SqliteAgentFactory>,
    agents: Arc<BTreeMap<String, AgentEntry>>,
    allowed: &[&str],
) -> TaskTool {
    let mut advertised: Vec<(String, String)> = list_available_agents()
        .into_iter()
        .filter(|a| allowed.is_empty() || allowed.contains(&a.name.as_str()))
        .map(|a| (a.name, a.description))
        .collect();
    for (name, entry) in agents.iter() {
        if allowed.is_empty() || allowed.contains(&name.as_str()) {
            advertised.push((name.clone(), entry.description.clone()));
        }
    }
    TaskTool::new(factory, advertised)
}

/// One workflow manager per profile: it validates specs, spawns runs, and
/// answers status/wait/cancel for the four workflow tools. Agents derive
/// from the same factory the task tool uses, so delegation stays one level
/// deep (composed agents never receive `task` or `workflow_*` tools).
fn workflow_manager(factory: Arc<SqliteAgentFactory>) -> Arc<WorkflowManager> {
    Arc::new(WorkflowManager::new(factory, list_agent_names()))
}

fn build_tool_registry_and_list(inputs: &ProfileInputs) -> Result<RegistryBundle> {
    let ProfileInputs {
        name,
        provider,
        pool,
        working_directory,
        additional_tools,
        skills,
        enforcement,
        context_limit,
        agents,
        ..
    } = inputs;
    let mut tools: Vec<Box<dyn Tool>> = match *name {
        "build" => {
            let factory = agent_factory(
                inputs.model_catalog.clone(),
                provider.clone(),
                pool.clone(),
                skills.clone(),
                working_directory.clone(),
                inputs.web_fetch.clone(),
                *enforcement,
                *context_limit,
                agents.clone(),
            );
            let workflows = workflow_manager(factory.clone());
            vec![
                Box::new(task_tool(factory, agents.clone(), &[])),
                Box::new(WorkflowStartTool::new(workflows.clone())),
                Box::new(WorkflowStatusTool::new(workflows.clone())),
                Box::new(WorkflowWaitTool::new(workflows.clone())),
                Box::new(WorkflowCancelTool::new(workflows)),
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
                Box::new(WebFetchTool::with_shared_policy(inputs.web_fetch.clone())),
                Box::new(ListTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(CodeSearchTool),
                Box::new(WebSearchTool),
                Box::new(TodoWriteTool),
                Box::new(TodoReadTool),
            ]
        }
        "plan" => {
            vec![
                Box::new(task_tool(
                    agent_factory(
                        inputs.model_catalog.clone(),
                        provider.clone(),
                        pool.clone(),
                        skills.clone(),
                        inputs.working_directory.clone(),
                        inputs.web_fetch.clone(),
                        *enforcement,
                        *context_limit,
                        agents.clone(),
                    ),
                    agents.clone(),
                    &["explore"],
                )),
                Box::new(ReadFileTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(GlobTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(GrepTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(ListTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(BashTool {
                    working_directory: working_directory.clone(),
                    safe_mode: false,
                }),
                Box::new(WebFetchTool::with_shared_policy(inputs.web_fetch.clone())),
                Box::new(WebSearchTool),
                Box::new(TodoWriteTool),
                Box::new(TodoReadTool),
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
                Box::new(WebFetchTool::with_shared_policy(inputs.web_fetch.clone())),
                Box::new(ListTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(CodeSearchTool),
                Box::new(WebSearchTool),
            ]
        }
        "general" => {
            let factory = agent_factory(
                inputs.model_catalog.clone(),
                provider.clone(),
                pool.clone(),
                skills.clone(),
                working_directory.clone(),
                inputs.web_fetch.clone(),
                *enforcement,
                *context_limit,
                agents.clone(),
            );
            let workflows = workflow_manager(factory.clone());
            vec![
                Box::new(task_tool(factory, agents.clone(), &[])),
                Box::new(WorkflowStartTool::new(workflows.clone())),
                Box::new(WorkflowStatusTool::new(workflows.clone())),
                Box::new(WorkflowWaitTool::new(workflows.clone())),
                Box::new(WorkflowCancelTool::new(workflows)),
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
                Box::new(WebFetchTool::with_shared_policy(inputs.web_fetch.clone())),
                Box::new(ListTool {
                    working_directory: working_directory.clone(),
                }),
                Box::new(CodeSearchTool),
                Box::new(WebSearchTool),
                Box::new(TodoWriteTool),
                Box::new(TodoReadTool),
            ]
        }
        _ => {
            let entry = agents
                .get(*name)
                .ok_or_else(|| anyhow!("Unknown agent type: {}", name))?;
            let tools_arc = build_custom_agent_tools(inputs, entry)?;
            let registry: Arc<dyn ToolRegistry> = Arc::new(ToolRegistryImpl {
                tools: Arc::new(tools_arc.clone()),
            });
            return Ok((registry, tools_arc));
        }
    };

    if !skills.is_empty() {
        tools.push(Box::new(SkillTool::new(skills.clone())));
    }

    let mut tools_arc: Vec<Arc<dyn Tool>> = tools
        .into_iter()
        .map(|t| Arc::from(t) as Arc<dyn Tool>)
        .collect();
    let mut names = tools_arc
        .iter()
        .map(|tool| tool.name().to_string())
        .collect::<std::collections::HashSet<_>>();
    for tool in additional_tools.iter() {
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
    // Additional tools (including MCP) have no read-only capability contract.
    // Never expose them in Plan, even indirectly through BatchTool.
    if *name != "plan" {
        tools_arc.extend(additional_tools.iter().cloned());
    }

    let registry: Arc<dyn ToolRegistry> = Arc::new(ToolRegistryImpl {
        tools: Arc::new(tools_arc.clone()),
    });

    Ok((registry, tools_arc))
}

/// Builds the tool set for a config-defined agent: the base profile's tools,
/// optionally narrowed by the entry's allowlist. Config agents never get
/// `task`, so delegation stays one level deep (matching the factory's
/// subagent depth limit).
fn build_custom_agent_tools(
    inputs: &ProfileInputs<'_>,
    entry: &AgentEntry,
) -> Result<Vec<Arc<dyn Tool>>> {
    // Rebuild the base profile's tool set to narrow against. The base name
    // is validated at config load, so this resolves a built-in. Subagents
    // never receive MCP/additional tools.
    let (_, base_tools) = build_tool_registry_and_list(&ProfileInputs {
        model_catalog: inputs.model_catalog.clone(),
        name: &entry.base,
        provider: inputs.provider.clone(),
        pool: inputs.pool.clone(),
        working_directory: inputs.working_directory.clone(),
        additional_tools: Vec::new(),
        skills: inputs.skills.clone(),
        web_fetch: inputs.web_fetch.clone(),
        enforcement: inputs.enforcement,
        context_limit: inputs.context_limit,
        agents: inputs.agents.clone(),
        session_id: inputs.session_id,
    })?;
    let tools: Vec<Arc<dyn Tool>> = match &entry.tools {
        None => base_tools
            .into_iter()
            .filter(|tool| tool.name() != "task")
            .collect(),
        Some(requested) => {
            anyhow::ensure!(
                !requested.iter().any(|tool| tool == "task"),
                "agent tool list cannot include 'task' (subagents cannot delegate)"
            );
            let known: std::collections::HashSet<_> =
                base_tools.iter().map(|tool| tool.name()).collect();
            for tool in requested {
                anyhow::ensure!(
                    known.contains(&tool.as_str()),
                    "base profile '{}' has no tool '{}' (agent tool lists narrow the base profile)",
                    entry.base,
                    tool
                );
            }
            base_tools
                .into_iter()
                .filter(|tool| requested.iter().any(|name| name == tool.name()))
                .collect()
        }
    };
    Ok(tools)
}

pub async fn create_agent(
    inputs: ProfileInputs<'_>,
    log: Option<RequestLog>,
) -> Result<(Agent, Arc<dyn ToolRegistry>)> {
    let ProfileInputs {
        model_catalog,
        name,
        provider,
        pool,
        additional_tools,
        skills,
        web_fetch,
        enforcement,
        context_limit,
        agents,
        session_id,
        ..
    } = inputs;
    let working_directory = std::env::current_dir()?;

    let memory = SqliteMemory::new(pool.clone(), session_id.to_string()).await?;

    let (all_tools, system_prompt, registry) = agent_profile(ProfileInputs {
        model_catalog,
        name,
        provider: provider.clone(),
        pool: pool.clone(),
        working_directory,
        additional_tools,
        skills,
        web_fetch,
        enforcement,
        context_limit,
        agents,
        session_id,
    })?;
    let mut builder = Agent::builder()
        .llm(provider)
        .with_tools(all_tools)
        .memory(Box::new(memory))
        .system_prompt(&system_prompt)
        .with_enforcement(enforcement)
        // Tools observe the real conversation identity (e.g. the task tool
        // records subagent lineage against it), not a random UUID.
        .session_id(session_id)
        // Per-session tool state (todo lists, ...) persists in SQLite under
        // the same session id, so it survives restarts and profile switches.
        .with_session_state(SessionState::new(Arc::new(SqliteSessionStateStore::new(
            pool.clone(),
        ))));
    // Restore the durable reminder state; a session without one (fresh, or
    // created before reminders were persisted) starts from the profile
    // default. The initial state is written immediately so it is durable
    // from the first turn.
    let reminder = match gewu_storage::get_reminder_opt(&pool, session_id).await? {
        Some(stored) => stored,
        None => {
            let initial = if name == "plan" {
                profile_reminder(None, name)
            } else {
                gewu_core::agent::Reminder::default()
            };
            gewu_storage::set_reminder(&pool, session_id, &initial).await?;
            initial
        }
    };
    builder = builder.with_reminder(reminder);
    if let Some(limit) = context_limit {
        builder = builder.context_limit(limit);
    }
    if let Some(log) = log {
        builder = builder.with_log(log);
    }
    Ok((builder.build()?, registry))
}

/// Build profile configuration without opening or reloading conversation memory.
///
/// `session_id` keys the conversation's memory and reminder storage; the task
/// tool no longer reads it from here (it receives the parent per call via the
/// tool context).
pub fn agent_profile(inputs: ProfileInputs) -> Result<ProfileBundle> {
    let ProfileInputs {
        model_catalog,
        name,
        provider,
        pool,
        working_directory,
        additional_tools,
        skills,
        web_fetch,
        enforcement,
        context_limit,
        agents,
        session_id,
    } = inputs;
    let (registry, tools_arc) = build_tool_registry_and_list(&ProfileInputs {
        model_catalog,
        name,
        provider,
        pool,
        working_directory,
        additional_tools,
        skills: skills.clone(),
        web_fetch,
        enforcement,
        context_limit,
        agents: agents.clone(),
        session_id,
    })?;

    let mut system_prompt = match agents.get(name) {
        Some(entry) => match &entry.instructions {
            // Config-defined instructions replace the base prompt entirely.
            Some(instructions) => instructions.clone(),
            None => base_profile_prompt(&entry.base)?,
        },
        None => base_profile_prompt(name)?,
    };
    if !skills.is_empty() {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(&gewu_tools::skill::available_skills_section(&skills));
    }

    Ok((tools_arc, system_prompt, registry))
}

/// The built-in system prompt for a profile name. `build` and `general`
/// extend the shared base with the background-workflow note (their profiles
/// carry the workflow tools; `plan` does not).
fn base_profile_prompt(name: &str) -> Result<String> {
    const WORKFLOW_NOTE: &str = include_str!("prompts/workflow-note.txt");
    Ok(match name {
        "build" => format!("{}\n\n{WORKFLOW_NOTE}", include_str!("prompts/build.txt")),
        "plan" => include_str!("prompts/build.txt").to_string(),
        "explore" => include_str!("prompts/explore.txt").to_string(),
        "general" => format!("{}\n\n{WORKFLOW_NOTE}", include_str!("prompts/general.txt")),
        _ => return Err(anyhow!("Unknown agent type: {}", name)),
    })
}

/// Mode instructions live in conversation reminders, not the base prompt.
pub fn profile_reminder(previous: Option<&str>, target: &str) -> gewu_core::agent::Reminder {
    use gewu_core::agent::Reminder;
    match target {
        "plan" => Reminder::switch(
            include_str!("prompts/switch-to-plan.txt"),
            Some(include_str!("prompts/plan-heartbeat.txt").into()),
        ),
        "build" if previous == Some("plan") => {
            Reminder::switch(include_str!("prompts/switch-to-build.txt"), None)
        }
        _ => Reminder::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn web_fetch_policy_reaches_every_profile_and_subagent() {
        let config: crate::config::Config = serde_json::from_str(
            r#"{"webFetch":{"allowedHosts":[" EXAMPLE.ORG. "]}}"#,
        ).unwrap();
        let policy = Arc::new(config.web_fetch.unwrap().policy());
        let pool = sqlx::sqlite::SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        for profile in ["build", "plan", "explore", "general"] {
            let mut inputs = test_inputs(profile, Arc::new(DummyLlm), pool.clone(), dir.path().into(), vec![], vec![], "parent");
            inputs.web_fetch = policy.clone();
            let (_, _, registry) = agent_profile(inputs).unwrap();
            let error = registry.execute("webfetch", serde_json::json!({"url":"http://127.0.0.1:9"}), &ToolContext::new("parent", "m")).await.unwrap_err();
            assert!(error.to_string().contains("not in the allowed hosts list"), "{profile}: {error}");
        }
        SqliteMemory::new(pool.clone(), "parent".into()).await.unwrap();
        let factory = SqliteAgentFactory {
            model_catalog: None, provider: Arc::new(DummyLlm), pool,
            skills: vec![], working_directory: std::env::current_dir().unwrap(), web_fetch: policy, enforcement: PathEnforcement::None, context_limit: None,
            agents: Arc::new(BTreeMap::new()), templates: Mutex::new(HashMap::new()),
        };
        let (child, _) = factory.create("explore", None, "parent").await.unwrap();
        let error = child.execute("webfetch", serde_json::json!({"url":"http://127.0.0.1:9"}), &ToolContext::new("child", "m")).await.unwrap_err();
        assert!(error.to_string().contains("not in the allowed hosts list"), "{error}");
    }

    #[tokio::test]
    async fn subagent_inherits_current_model_and_resume_keeps_its_selection() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        SqliteMemory::new(pool.clone(), "parent".into())
            .await
            .unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);
        let entries = [("old", 10_000), ("new", 50_000)]
            .into_iter()
            .map(|(key, context_limit)| {
                (
                    key.into(),
                    crate::bootstrap::ModelEntry {
                        provider: provider.clone(),
                        provider_name: key.into(),
                        label: key.into(),
                        context_limit,
                    },
                )
            })
            .collect();
        let factory = SqliteAgentFactory {
            model_catalog: Some(ModelCatalog {
                entries: Arc::new(entries),
                default: "old".into(),
            }),
            provider,
            pool: pool.clone(),
            skills: vec![],
            working_directory: std::env::current_dir().unwrap(),
            web_fetch: Arc::default(),
            enforcement: PathEnforcement::None,
            context_limit: Some(10_000),
            agents: Arc::new(BTreeMap::new()),
            templates: Mutex::new(HashMap::new()),
        };
        gewu_storage::set_session_model(&pool, "parent", "new")
            .await
            .unwrap();
        let (child, id) = factory.create("explore", None, "parent").await.unwrap();
        assert_eq!(child.context_limit, 50_000);
        assert_eq!(
            gewu_storage::session_model(&pool, &id)
                .await
                .unwrap()
                .as_deref(),
            Some("new")
        );
        gewu_storage::set_session_model(&pool, "parent", "old")
            .await
            .unwrap();
        let (resumed, _) = factory.create("explore", Some(id), "parent").await.unwrap();
        assert_eq!(resumed.context_limit, 50_000);
        gewu_storage::set_session_model(&pool, "parent", "missing")
            .await
            .unwrap();
        assert!(factory.create("explore", None, "parent").await.is_err());
    }

    /// Builds `ProfileInputs` for tests; session defaults to "session".
    fn test_inputs<'a>(
        name: &'a str,
        provider: Arc<dyn LlmProvider>,
        pool: SqlitePool,
        working_directory: std::path::PathBuf,
        additional_tools: Vec<Arc<dyn Tool>>,
        skills: Vec<SkillInfo>,
        session_id: &'a str,
    ) -> ProfileInputs<'a> {
        ProfileInputs {
            model_catalog: None,
            name,
            provider,
            pool,
            working_directory,
            additional_tools,
            skills,
            web_fetch: Arc::default(),
            enforcement: PathEnforcement::Contain,
            context_limit: None,
            agents: Arc::new(BTreeMap::new()),
            session_id,
        }
    }

    /// A config-defined agent entry for tests.
    fn custom_entry(base: &str, instructions: Option<&str>, tools: Option<&[&str]>) -> AgentEntry {
        AgentEntry {
            base: base.to_string(),
            description: "test agent".to_string(),
            instructions: instructions.map(str::to_string),
            tools: tools.map(|list| list.iter().map(|t| t.to_string()).collect()),
        }
    }

    #[tokio::test]
    async fn build_and_plan_share_base_prompt() {
        let pool = SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut prompts = Vec::new();
        for profile in ["build", "plan"] {
            let (_, prompt, _) = agent_profile(test_inputs(
                profile,
                Arc::new(DummyLlm),
                pool.clone(),
                dir.path().to_path_buf(),
                vec![],
                vec![],
                "session",
            ))
            .unwrap();
            prompts.push(prompt);
        }
        assert!(prompts[0].starts_with(prompts[1].as_str()));
    }

    #[test]
    fn profile_reminders_follow_mode_transitions() {
        use gewu_core::types::{ContentPart, Message};
        for (previous, target, expected) in [
            (None, "plan", Some("Plan mode is active")),
            (Some("build"), "plan", Some("Plan mode is active")),
            (Some("plan"), "build", Some("no longer in read-only mode")),
            (Some("build"), "build", None),
        ] {
            let mut reminder = profile_reminder(previous, target);
            reminder.on_user_turn();
            let mut messages = vec![Message::User { content: vec![] }];
            reminder.apply(&mut messages);
            let Message::User { content } = &messages[0] else {
                unreachable!()
            };
            match expected {
                Some(expected) => assert!(
                    matches!(&content[0], ContentPart::Reminder { text } if text.contains(expected))
                ),
                None => assert!(content.is_empty()),
            }
            reminder.on_user_turn();
            let mut next = vec![Message::User { content: vec![] }];
            reminder.apply(&mut next);
            assert_eq!(next, vec![Message::User { content: vec![] }]);
        }
    }

    #[tokio::test]
    async fn plan_has_no_editing_tools() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (_, tools) = build_tool_registry_and_list(&test_inputs(
            "plan",
            Arc::new(DummyLlm),
            pool,
            dir.path().to_path_buf(),
            vec![Arc::new(NamedTool("external_mutator"))],
            vec![],
            "session",
        ))
        .unwrap();
        let names: Vec<_> = tools.iter().map(|tool| tool.name()).collect();
        for expected in [
            "read_file",
            "glob",
            "grep",
            "list",
            "webfetch",
            "websearch",
            "bash",
            "task",
            "todowrite",
            "todoread",
        ] {
            assert!(names.contains(&expected), "Plan is missing {expected}");
        }
        for forbidden in ["write_file", "edit_file", "apply_patch", "external_mutator"] {
            assert!(!names.contains(&forbidden), "Plan exposes {forbidden}");
        }
        // Delegation is restricted to the read-only explore subagent.
        let task = tools
            .iter()
            .find(|tool| tool.name() == "task")
            .expect("plan should expose the task tool");
        let description = task.description();
        assert!(
            description.contains("\"explore\""),
            "plan task tool should advertise explore"
        );
        assert!(
            !description.contains("\"build\""),
            "plan task tool must not advertise build"
        );
        assert!(
            !description.contains("\"general\""),
            "plan task tool must not advertise general"
        );
        // Bash is unrestricted (prompt-restricted to read-only, like
        // opencode's plan mode): a command outside the old safe-mode
        // allow-list executes instead of being refused.
        let bash = tools
            .iter()
            .find(|tool| tool.name() == "bash")
            .expect("plan should expose bash");
        let result = bash
            .execute(
                serde_json::json!({ "command": "touch plan_probe" }),
                &ToolContext::new("s", "m"),
            )
            .await
            .unwrap();
        assert!(
            !result.output.contains("not allowed in safe mode"),
            "plan bash should not run in safe mode, got: {}",
            result.output
        );
        assert!(
            dir.path().join("plan_probe").exists(),
            "plan bash should execute commands outside the safe-mode allow-list"
        );
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
            let (_, tools) = build_tool_registry_and_list(&test_inputs(
                profile,
                provider.clone(),
                pool.clone(),
                directory.path().to_path_buf(),
                Vec::new(),
                Vec::new(),
                "session",
            ))
            .unwrap();
            assert!(tools.iter().any(|tool| tool.name() == "apply_patch"));
        }

        let (_, tools) = build_tool_registry_and_list(&test_inputs(
            "explore",
            provider,
            pool,
            directory.path().to_path_buf(),
            Vec::new(),
            Vec::new(),
            "session",
        ))
        .unwrap();
        assert!(!tools.iter().any(|tool| tool.name() == "apply_patch"));
    }

    #[tokio::test]
    async fn create_agent_accepts_request_log() {
        let pool = SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);
        let log: RequestLog = Arc::new(Mutex::new(Box::new(Vec::<u8>::new())));

        let (agent, _) = create_agent(
            test_inputs(
                "build",
                provider,
                pool,
                std::env::current_dir().unwrap(),
                Vec::new(),
                Vec::new(),
                "session",
            ),
            Some(log),
        )
        .await
        .expect("create_agent should accept a request log");

        assert!(!agent.list_tools().is_empty());
    }

    /// Restored sessions resume their persisted reminder state (nothing
    /// pending, cadence aligned) instead of re-issuing the switch reminder.
    #[tokio::test]
    async fn create_agent_restores_persisted_reminder_state() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);

        let stored = gewu_core::agent::Reminder::continue_at("FULL", Some("SPARSE".into()), 9);
        gewu_storage::set_reminder(&pool, "session", &stored)
            .await
            .unwrap();
        let cwd = std::env::current_dir().unwrap();
        let (agent, _) = create_agent(
            test_inputs(
                "plan",
                provider.clone(),
                pool.clone(),
                cwd.clone(),
                Vec::new(),
                Vec::new(),
                "session",
            ),
            None,
        )
        .await
        .unwrap();
        assert_eq!(agent.reminder().user_turns(), 9);
        assert!(agent.reminder().pending().is_none());

        // A session without a stored state starts from the profile default.
        let (agent, _) = create_agent(
            test_inputs("plan", provider, pool, cwd, Vec::new(), Vec::new(), "fresh"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(agent.reminder().user_turns(), 0);
        assert!(agent.reminder().pending().is_some());
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
            test_inputs(
                "build",
                provider,
                pool,
                std::env::current_dir().unwrap(),
                Vec::new(),
                skills,
                "session",
            ),
            None,
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

        let reserved = build_tool_registry_and_list(&test_inputs(
            "explore",
            provider.clone(),
            pool.clone(),
            directory.path().to_path_buf(),
            vec![Arc::new(NamedTool("apply_patch"))],
            Vec::new(),
            "session",
        ));
        assert!(reserved.err().unwrap().to_string().contains("reserved"));

        let duplicate = build_tool_registry_and_list(&test_inputs(
            "build",
            provider,
            pool,
            directory.path().to_path_buf(),
            vec![Arc::new(NamedTool("websearch"))],
            Vec::new(),
            "session",
        ));
        assert!(
            duplicate
                .err()
                .unwrap()
                .to_string()
                .contains("Duplicate tool name")
        );
    }

    #[tokio::test]
    async fn task_factory_records_subagents_and_enforces_depth() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);

        let factory = SqliteAgentFactory {
            model_catalog: None,
            provider: provider.clone(),
            pool: pool.clone(),
            skills: Vec::new(),
            working_directory: std::env::current_dir().unwrap(),
            web_fetch: Arc::default(),
            enforcement: PathEnforcement::None,
            context_limit: None,
            agents: Arc::new(BTreeMap::new()),
            templates: Mutex::new(HashMap::new()),
        };
        // The parent conversation is passed per call, not held by the factory.
        let (_, subagent_id) = factory.create("explore", None, "root").await.unwrap();
        // The subagent session is recorded with its parent, so the session
        // list can hide it while task resume keeps working.
        assert_eq!(
            gewu_storage::subagent_depth(&pool, "root").await.unwrap(),
            0
        );
        assert_eq!(
            gewu_storage::subagent_depth(&pool, &subagent_id)
                .await
                .unwrap(),
            1
        );

        // A subagent cannot launch further subagents.
        let error = match factory.create("explore", None, &subagent_id).await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("nested subagent spawn should be rejected"),
        };
        assert!(error.contains("depth limit"), "unexpected error: {error}");
    }

    #[tokio::test]
    async fn custom_agents_are_advertised_to_delegating_profiles_only() {
        let pool = SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        let dir = tempdir().unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);
        let agents = Arc::new(BTreeMap::from([(
            "reviewer".to_string(),
            custom_entry("general", Some("Be meticulous."), None),
        )]));

        let (_, tools) = build_tool_registry_and_list(&ProfileInputs {
            agents,
            ..test_inputs(
                "build",
                provider.clone(),
                pool.clone(),
                dir.path().to_path_buf(),
                vec![],
                vec![],
                "session",
            )
        })
        .unwrap();
        let task = tools
            .iter()
            .find(|tool| tool.name() == "task")
            .expect("build exposes task");
        assert!(
            task.description().contains("\"reviewer\""),
            "build's task tool should advertise config agents"
        );

        let (_, tools) = build_tool_registry_and_list(&ProfileInputs {
            agents: Arc::new(BTreeMap::from([(
                "reviewer".to_string(),
                custom_entry("general", None, None),
            )])),
            ..test_inputs(
                "plan",
                provider,
                pool,
                dir.path().to_path_buf(),
                vec![],
                vec![],
                "session",
            )
        })
        .unwrap();
        let task = tools
            .iter()
            .find(|tool| tool.name() == "task")
            .expect("plan exposes task");
        assert!(
            !task.description().contains("\"reviewer\""),
            "plan's task tool must not advertise config agents"
        );
    }

    #[tokio::test]
    async fn custom_agent_tools_narrow_the_base_profile() {
        let pool = SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        let dir = tempdir().unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);
        let inputs = |name: &'static str, entry: AgentEntry| ProfileInputs {
            agents: Arc::new(BTreeMap::from([(name.to_string(), entry)])),
            ..test_inputs(
                name,
                provider.clone(),
                pool.clone(),
                dir.path().to_path_buf(),
                vec![],
                vec![],
                "session",
            )
        };

        // An explicit tool list narrows the base set.
        let (_, tools) = build_tool_registry_and_list(&inputs(
            "reviewer",
            custom_entry("general", None, Some(&["read_file", "grep"])),
        ))
        .unwrap();
        let names: Vec<_> = tools.iter().map(|tool| tool.name()).collect();
        assert!(names.contains(&"read_file") && names.contains(&"grep"));
        for forbidden in ["task", "bash", "write_file", "webfetch", "todowrite"] {
            assert!(
                !names.contains(&forbidden),
                "custom agent exposes {forbidden}"
            );
        }

        // Without a tool list the base set survives, minus task.
        let (_, tools) =
            build_tool_registry_and_list(&inputs("writer", custom_entry("general", None, None)))
                .unwrap();
        let names: Vec<_> = tools.iter().map(|tool| tool.name()).collect();
        assert!(names.contains(&"bash"));
        assert!(!names.contains(&"task"), "custom agents never delegate");

        // Unknown tool names are rejected instead of silently dropped.
        let error = match build_tool_registry_and_list(&inputs(
            "bad",
            custom_entry("general", None, Some(&["read_file", "nonexistent"])),
        )) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("unknown tool names should be rejected"),
        };
        assert!(error.contains("nonexistent"), "unexpected error: {error}");

        // Requesting task is rejected outright.
        let error = match build_tool_registry_and_list(&inputs(
            "bad",
            custom_entry("general", None, Some(&["task"])),
        )) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("requesting task should be rejected"),
        };
        assert!(
            error.contains("cannot delegate"),
            "unexpected error: {error}"
        );

        // Unknown agent names still fail.
        let error = match build_tool_registry_and_list(&test_inputs(
            "missing",
            provider.clone(),
            pool.clone(),
            dir.path().to_path_buf(),
            vec![],
            vec![],
            "session",
        )) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("unknown agent names should be rejected"),
        };
        assert!(
            error.contains("Unknown agent type"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn custom_agent_instructions_replace_the_base_prompt() {
        let pool = SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        let dir = tempdir().unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);
        let (_, general_prompt, _) = agent_profile(test_inputs(
            "general",
            provider.clone(),
            pool.clone(),
            dir.path().to_path_buf(),
            vec![],
            vec![],
            "session",
        ))
        .unwrap();

        // Instructions replace the base prompt entirely.
        let (_, prompt, _) = agent_profile(ProfileInputs {
            agents: Arc::new(BTreeMap::from([(
                "reviewer".to_string(),
                custom_entry("general", Some("Be meticulous."), None),
            )])),
            ..test_inputs(
                "reviewer",
                provider.clone(),
                pool.clone(),
                dir.path().to_path_buf(),
                vec![],
                vec![],
                "session",
            )
        })
        .unwrap();
        assert_eq!(prompt, "Be meticulous.");

        // Without instructions the base profile's prompt applies.
        let (_, prompt, _) = agent_profile(ProfileInputs {
            agents: Arc::new(BTreeMap::from([(
                "reviewer".to_string(),
                custom_entry("general", None, None),
            )])),
            ..test_inputs(
                "reviewer",
                provider,
                pool,
                dir.path().to_path_buf(),
                vec![],
                vec![],
                "session",
            )
        })
        .unwrap();
        assert_eq!(prompt, general_prompt);
    }

    #[tokio::test]
    async fn factory_creates_config_agents() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);
        let factory = SqliteAgentFactory {
            model_catalog: None,
            provider: provider.clone(),
            pool: pool.clone(),
            skills: Vec::new(),
            working_directory: std::env::current_dir().unwrap(),
            web_fetch: Arc::default(),
            enforcement: PathEnforcement::None,
            context_limit: None,
            agents: Arc::new(BTreeMap::from([(
                "reviewer".to_string(),
                custom_entry("general", Some("Be meticulous."), Some(&["read_file"])),
            )])),
            templates: Mutex::new(HashMap::new()),
        };
        let (agent, session_id) = factory.create("reviewer", None, "parent").await.unwrap();
        assert!(agent.list_tools().iter().any(|name| name == "read_file"));
        assert!(!agent.list_tools().iter().any(|name| name == "task"));
        // Recorded as a regular subagent of the parent conversation.
        assert_eq!(
            gewu_storage::subagent_depth(&pool, "parent").await.unwrap(),
            0
        );
        assert_eq!(
            gewu_storage::subagent_depth(&pool, &session_id)
                .await
                .unwrap(),
            1
        );
    }

    /// Tool state written in one spawn must be visible to a later spawn of
    /// the same session (task resume), because the template bakes in the
    /// pool-backed session state.
    #[tokio::test]
    async fn resumed_subagents_recover_their_todo_state() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        SqliteMemory::new(pool.clone(), "parent".into())
            .await
            .unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);
        let factory = SqliteAgentFactory {
            model_catalog: None,
            provider,
            pool: pool.clone(),
            skills: Vec::new(),
            working_directory: std::env::current_dir().unwrap(),
            web_fetch: Arc::default(),
            enforcement: PathEnforcement::None,
            context_limit: None,
            agents: Arc::new(BTreeMap::new()),
            templates: Mutex::new(HashMap::new()),
        };

        let (writer, session_id) = factory.create("build", None, "parent").await.unwrap();
        let todos = vec![gewu_tools::TodoItem {
            id: "1".into(),
            content: "first".into(),
            status: "pending".into(),
            priority: None,
        }];
        writer
            .session_state()
            .write(&session_id, "todos", &todos)
            .await
            .unwrap();
        drop(writer);

        let (reader, resumed_id) = factory
            .create("build", Some(session_id.clone()), "parent")
            .await
            .unwrap();
        assert_eq!(resumed_id, session_id);
        let restored: Vec<gewu_tools::TodoItem> = reader
            .session_state()
            .read(&session_id, "todos")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].content, "first");

        // The stateless todoread tool sees the restored list through the
        // session state on its tool context.
        let ctx =
            ToolContext::new(&session_id, "m").with_session_state(reader.session_state().clone());
        let result = reader
            .execute("todoread", serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(result.output.contains("first"), "{}", result.output);
    }

    #[tokio::test]
    async fn workflow_tools_are_only_in_delegating_profiles() {
        let pool = SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        let dir = tempdir().unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);

        for profile in ["build", "general"] {
            let (_, tools) = build_tool_registry_and_list(&test_inputs(
                profile,
                provider.clone(),
                pool.clone(),
                dir.path().to_path_buf(),
                vec![],
                vec![],
                "session",
            ))
            .unwrap();
            for expected in [
                "workflow_start",
                "workflow_status",
                "workflow_wait",
                "workflow_cancel",
            ] {
                assert!(
                    tools.iter().any(|tool| tool.name() == expected),
                    "{profile} is missing {expected}"
                );
            }
        }

        for profile in ["explore", "plan"] {
            let (_, tools) = build_tool_registry_and_list(&test_inputs(
                profile,
                provider.clone(),
                pool.clone(),
                dir.path().to_path_buf(),
                vec![],
                vec![],
                "session",
            ))
            .unwrap();
            for forbidden in [
                "workflow_start",
                "workflow_status",
                "workflow_wait",
                "workflow_cancel",
            ] {
                assert!(
                    !tools.iter().any(|tool| tool.name() == forbidden),
                    "{profile} exposes {forbidden}"
                );
            }
        }
    }

    #[tokio::test]
    async fn workflow_composed_agents_lack_delegation_tools() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);
        let factory = agent_factory(
            None,
            provider,
            pool,
            Vec::new(),
            std::path::PathBuf::from("."),
            Arc::default(),
            PathEnforcement::None,
            None,
            Arc::new(BTreeMap::new()),
        );

        // A workflow agent derived from build never carries the delegation
        // tools, so delegation stays one level deep.
        let (agent, _) = factory
            .create_with("build", &AgentOverrides::default(), "root")
            .await
            .unwrap();
        let names = agent.list_tools();
        for forbidden in COMPOSED_AGENT_FORBIDDEN_TOOLS {
            assert!(
                !names.iter().any(|name| name == forbidden),
                "workflow agent exposes {forbidden}"
            );
        }
        assert!(
            names.iter().any(|name| name == "read_file"),
            "the base profile's tools survive the strip"
        );

        // Inline tool lists narrow the (stripped) base: a tool the base
        // profile lacks — including forbidden delegation tools — is
        // rejected before anything spawns.
        let error = factory
            .validate_overrides(
                "explore",
                &AgentOverrides {
                    tools: Some(vec!["write_file".to_string()]),
                    instructions: None,
                },
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("no tool 'write_file'"),
            "{error}"
        );

        let error = match factory
            .create_with(
                "build",
                &AgentOverrides {
                    tools: Some(vec!["task".to_string()]),
                    instructions: None,
                },
                "root",
            )
            .await
        {
            Err(error) => error.to_string(),
            Ok(_) => panic!("requesting a forbidden tool should be rejected"),
        };
        assert!(error.contains("no tool 'task'"), "{error}");
    }

    #[tokio::test]
    async fn workflow_inline_instructions_extend_the_base_prompt() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyLlm);
        let factory = agent_factory(
            None,
            provider,
            pool,
            Vec::new(),
            std::path::PathBuf::from("."),
            Arc::default(),
            PathEnforcement::None,
            None,
            Arc::new(BTreeMap::new()),
        );

        let (agent, _) = factory
            .create_with(
                "explore",
                &AgentOverrides {
                    instructions: Some("Be terse.".to_string()),
                    tools: None,
                },
                "root",
            )
            .await
            .unwrap();
        let expected = format!("{}\n\nBe terse.", base_profile_prompt("explore").unwrap());
        assert_eq!(agent.template().system_prompt(), expected);
    }
}
