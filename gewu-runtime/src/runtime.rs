use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::StreamExt;
use gewu_core::Agent;
use gewu_core::traits::{
    AssemblyContext, LlmProvider, Plugin, Tool, ToolFactory, ToolRegistry,
};
use gewu_core::types::{
    AgentEvent, SpecialistAgentEventRecord, SpecialistDescriptor, SpecialistExecutionEnvelope,
    SpecialistExecutionRequest, SpecialistLlmIoRecord, TaskStatus, ToolContext,
    ToolExecutionRecord, ToolResult,
};
use gewu_storage::{SqliteMemory, TaskStore};
use gewu_tools::{BatchTool, report_result::ReportResultArgs};
use serde_json::Value;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::capability::{CapabilityRegistry, ToolFactoryProvider};
use crate::composer::{ComposerContext, ComposerToolFactory, composer_specialist_descriptor};
use crate::registry::{AgentConfig, BuiltinToolFactory};

pub struct GeWuRuntime {
    factories: Vec<Box<dyn ToolFactory>>,
    capability_registry: Arc<CapabilityRegistry>,
    specialists: Vec<SpecialistDescriptor>,
    provider: Arc<dyn LlmProvider>,
    pool: SqlitePool,
}

impl GeWuRuntime {
    pub fn builder() -> GeWuRuntimeBuilder {
        GeWuRuntimeBuilder {
            factories: Vec::new(),
            extra_providers: Vec::new(),
            specialists: Vec::new(),
            manifest_dirs: Vec::new(),
            provider: None,
            pool: None,
        }
    }

    pub fn find_specialist(&self, name: &str) -> Option<&SpecialistDescriptor> {
        self.specialists.iter().find(|d| d.id == name)
    }

    pub fn list_specialists(&self) -> Vec<AgentConfig> {
        self.specialists
            .iter()
            .map(|d| AgentConfig {
                name: d.id.clone(),
                description: d.description.clone(),
            })
            .collect()
    }

    /// Access the runtime's capability registry. Used by manifest-driven paths
    /// and for diagnostics (e.g. `gewu list capabilities`).
    pub fn capability_registry(&self) -> &CapabilityRegistry {
        &self.capability_registry
    }

    fn resolve_capabilities_with_working_directory(
        &self,
        caps: &[String],
        working_directory: PathBuf,
        metadata_json: Value,
    ) -> Result<Vec<Box<dyn Tool>>> {
        let ctx = AssemblyContext {
            working_directory,
            provider: self.provider.clone(),
            pool: self.pool.clone(),
            metadata_json,
        };

        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        for cap in caps {
            let mut resolved = false;
            for factory in &self.factories {
                if factory.capabilities().contains(cap) {
                    let produced = factory
                        .create_tools(cap, &ctx)
                        .map_err(|e| anyhow!("Factory failed for '{}': {}", cap, e))?;
                    tools.extend(produced);
                    resolved = true;
                    break;
                }
            }
            if !resolved {
                return Err(anyhow!("No factory can resolve capability: {}", cap));
            }
        }
        Ok(tools)
    }

    pub fn resolve_capabilities(&self, caps: &[String]) -> Result<Vec<Box<dyn Tool>>> {
        self.resolve_capabilities_with_working_directory(caps, std::env::current_dir()?, Value::Null)
    }

    pub fn resolve_capabilities_in_directory(
        &self,
        caps: &[String],
        working_directory: &PathBuf,
    ) -> Result<Vec<Box<dyn Tool>>> {
        self.resolve_capabilities_with_working_directory(
            caps,
            working_directory.clone(),
            Value::Null,
        )
    }

    pub fn resolve_capabilities_with_metadata(
        &self,
        caps: &[String],
        working_directory: &PathBuf,
        metadata_json: Value,
    ) -> Result<Vec<Box<dyn Tool>>> {
        self.resolve_capabilities_with_working_directory(
            caps,
            working_directory.clone(),
            metadata_json,
        )
    }

    pub async fn build_specialist(
        &self,
        name: &str,
        session_id: String,
        additional_tools: Vec<Arc<dyn Tool>>,
    ) -> Result<(Agent, Arc<dyn ToolRegistry>)> {
        self.build_specialist_with_metadata(name, session_id, additional_tools, Value::Null)
            .await
    }

    pub async fn build_specialist_with_metadata(
        &self,
        name: &str,
        session_id: String,
        additional_tools: Vec<Arc<dyn Tool>>,
        metadata_json: Value,
    ) -> Result<(Agent, Arc<dyn ToolRegistry>)> {
        let descriptor = self
            .find_specialist(name)
            .ok_or_else(|| anyhow!("Unknown specialist: {}", name))?;

        let tools = self.resolve_capabilities_with_metadata(
            &descriptor.capabilities,
            &std::env::current_dir()?,
            metadata_json,
        )?;
        let memory = SqliteMemory::new(self.pool.clone(), session_id).await?;

        let mut tools_arc: Vec<Arc<dyn Tool>> = tools
            .into_iter()
            .map(|t| Arc::from(t) as Arc<dyn Tool>)
            .collect();
        tools_arc.extend(additional_tools);

        let tools_for_registry = Arc::new(tools_arc.clone());
        let registry: Arc<dyn ToolRegistry> = Arc::new(RuntimeToolRegistry {
            tools: tools_for_registry,
        });

        let batch_tool: Arc<dyn Tool> = Arc::new(BatchTool::new(registry.clone()));
        let mut all_tools = vec![batch_tool];
        all_tools.extend(tools_arc);

        let agent = Agent::builder()
            .llm(self.provider.clone())
            .with_tools(all_tools)
            .memory(Box::new(memory))
            .system_prompt(&descriptor.system_prompt)
            .build()?;

        Ok((agent, registry))
    }

    pub async fn execute_request(
        &self,
        request: &SpecialistExecutionRequest,
        additional_tools: Vec<Arc<dyn Tool>>,
    ) -> Result<SpecialistExecutionEnvelope> {
        let store = TaskStore::new(self.pool.clone()).await?;
        let existing = store.get_task(&request.task_id).await?;
        if existing.is_none() {
            store
                .create_task(&request.task_id, &request.specialist_id, &request.prompt)
                .await?;
        }

        let descriptor = self
            .find_specialist(&request.specialist_id)
            .ok_or_else(|| anyhow!("Unknown specialist: {}", request.specialist_id))?;
        let tools = self.resolve_capabilities_with_metadata(
            &descriptor.capabilities,
            &std::env::current_dir()?,
            request.metadata_json.clone(),
        )?;
        let memory = SqliteMemory::new(self.pool.clone(), request.session_id.clone()).await?;

        let mut tools_arc: Vec<Arc<dyn Tool>> = tools
            .into_iter()
            .map(|t| Arc::from(t) as Arc<dyn Tool>)
            .collect();
        tools_arc.extend(additional_tools);

        let tools_for_registry = Arc::new(tools_arc.clone());
        let registry: Arc<dyn ToolRegistry> = Arc::new(RuntimeToolRegistry {
            tools: tools_for_registry,
        });

        let batch_tool: Arc<dyn Tool> = Arc::new(BatchTool::new(registry.clone()));
        let mut all_tools = vec![batch_tool];
        all_tools.extend(tools_arc);

        let mut agent = Agent::builder()
            .llm(self.provider.clone())
            .with_tools(all_tools)
            .memory(Box::new(memory))
            .system_prompt(&descriptor.system_prompt)
            .build()?;

        store
            .update_status(&request.task_id, &TaskStatus::Running, None)
            .await?;
        store.increment_attempt(&request.task_id).await?;

        let mut stream = Box::pin(agent.ask(&request.prompt));
        let mut output_json: Option<Value> = None;
        let mut result_text = String::new();
        let mut final_status = TaskStatus::Succeeded;
        let mut block_reason: Option<String> = None;
        let mut report_received = false;
        let mut tool_outputs: Vec<ToolExecutionRecord> = Vec::new();
        let mut events: Vec<SpecialistAgentEventRecord> = Vec::new();
        let mut final_error: Option<String> = None;

        while let Some(event_result) = stream.next().await {
            match event_result {
                Ok(event) => match event {
                    AgentEvent::ContentDelta { delta } => {
                        events.push(SpecialistAgentEventRecord {
                            event_type: "content_delta".to_string(),
                            tool_name: None,
                            payload_json: serde_json::json!({ "delta": delta }),
                        });
                        result_text.push_str(&delta);
                    }
                    AgentEvent::ReasoningDelta { delta } => {
                        events.push(SpecialistAgentEventRecord {
                            event_type: "reasoning_delta".to_string(),
                            tool_name: None,
                            payload_json: serde_json::json!({ "delta": delta }),
                        });
                    }
                    AgentEvent::ToolStart { tool, input } => {
                        events.push(SpecialistAgentEventRecord {
                            event_type: "tool_start".to_string(),
                            tool_name: Some(tool),
                            payload_json: serde_json::json!({ "input": input }),
                        });
                    }
                    AgentEvent::Finish { response } => {
                        events.push(SpecialistAgentEventRecord {
                            event_type: "finish".to_string(),
                            tool_name: None,
                            payload_json: serde_json::json!({ "response": response }),
                        });
                        if !report_received {
                            result_text = response;
                        }
                    }
                    AgentEvent::ToolFinish { tool, output } if tool == "report_result" => {
                        events.push(SpecialistAgentEventRecord {
                            event_type: "tool_finish".to_string(),
                            tool_name: Some(tool.clone()),
                            payload_json: serde_json::json!({ "output": output }),
                        });
                        tool_outputs.push(ToolExecutionRecord {
                            tool_name: tool.clone(),
                            output: output.clone(),
                        });
                        report_received = true;
                        if let Ok(report) = serde_json::from_str::<ReportResultArgs>(&output) {
                            match report.status.as_str() {
                                "blocked" => {
                                    final_status = TaskStatus::Blocked;
                                    block_reason = report.block_reason.clone();
                                    result_text = report.block_reason.unwrap_or_default();
                                    output_json = None;
                                }
                                _ => {
                                    final_status = TaskStatus::Succeeded;
                                    output_json = report.result;
                                }
                            }
                        }
                    }
                    AgentEvent::ToolFinish { tool, output } => {
                        events.push(SpecialistAgentEventRecord {
                            event_type: "tool_finish".to_string(),
                            tool_name: Some(tool.clone()),
                            payload_json: serde_json::json!({ "output": output }),
                        });
                        tool_outputs.push(ToolExecutionRecord {
                            tool_name: tool,
                            output,
                        });
                    }
                    AgentEvent::Error { error } => {
                        events.push(SpecialistAgentEventRecord {
                            event_type: "error".to_string(),
                            tool_name: None,
                            payload_json: serde_json::json!({ "error": error }),
                        });
                        final_status = TaskStatus::Blocked;
                        block_reason = Some(error.clone());
                        final_error = Some(error.clone());
                        result_text = error;
                        output_json = None;
                    }
                },
                Err(e) => {
                    final_status = TaskStatus::Failed;
                    final_error = Some(e.to_string());
                    result_text = e.to_string();
                    output_json = None;
                    break;
                }
            }
        }

        store
            .update_status(&request.task_id, &final_status, block_reason.as_deref())
            .await?;
        if final_status == TaskStatus::Succeeded {
            let stored = output_json
                .clone()
                .unwrap_or_else(|| Value::String(result_text.clone()));
            store.set_result_json(&request.task_id, &stored).await?;
        }

        let attempt = store
            .get_task(&request.task_id)
            .await?
            .map(|r| r.attempt)
            .unwrap_or(0);

        Ok(SpecialistExecutionEnvelope {
            task_id: request.task_id.clone(),
            session_id: request.session_id.clone(),
            specialist_id: request.specialist_id.clone(),
            status: final_status,
            attempt,
            output_json: if block_reason.is_some() {
                None
            } else {
                Some(output_json.unwrap_or_else(|| Value::String(result_text.clone())))
            },
            block_reason,
            tool_outputs,
            events,
            llm_io: vec![SpecialistLlmIoRecord {
                system_prompt: descriptor.system_prompt.clone(),
                user_prompt: request.prompt.clone(),
                final_response: Some(result_text),
                error: final_error,
            }],
        })
    }
}

struct RuntimeToolRegistry {
    tools: Arc<Vec<Arc<dyn Tool>>>,
}

#[async_trait]
impl ToolRegistry for RuntimeToolRegistry {
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

pub struct GeWuRuntimeBuilder {
    factories: Vec<Box<dyn ToolFactory>>,
    extra_providers: Vec<(String, String, Box<dyn ToolFactory>)>,
    specialists: Vec<SpecialistDescriptor>,
    manifest_dirs: Vec<PathBuf>,
    provider: Option<Arc<dyn LlmProvider>>,
    pool: Option<SqlitePool>,
}

impl GeWuRuntimeBuilder {
    pub fn provider(mut self, p: Arc<dyn LlmProvider>) -> Self {
        self.provider = Some(p);
        self
    }

    pub fn pool(mut self, p: SqlitePool) -> Self {
        self.pool = Some(p);
        self
    }

    pub fn register_builtin_capabilities(mut self) -> Self {
        self.factories.push(Box::new(BuiltinToolFactory));
        self.extra_providers
            .push(("builtin".into(), "builtin".into(), Box::new(BuiltinToolFactory)));
        self
    }

    pub fn register_builtin_specialists(mut self) -> Self {
        self.specialists.extend(crate::registry::specialist_descriptors());
        self
    }

    pub fn with_manifest_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.manifest_dirs.push(dir.into());
        self
    }

    pub fn register_plugin(mut self, plugin: Box<dyn Plugin>) -> Self {
        let name = plugin.name().to_string();
        for factory in plugin.tool_factories() {
            self.factories.push(factory);
        }
        for factory in plugin.tool_factories() {
            self.extra_providers
                .push(("plugin".into(), name.clone(), factory));
        }
        self.specialists.extend(plugin.specialist_descriptors());
        self
    }

    /// Register factories and specialists from a borrowed plugin reference.
    /// Used by RuntimeFactory when iterating over PluginHost's plugins.
    pub fn register_plugin_from_host(mut self, plugin: &dyn Plugin) -> Self {
        let name = plugin.name().to_string();
        for factory in plugin.tool_factories() {
            self.factories.push(factory);
        }
        for factory in plugin.tool_factories() {
            self.extra_providers
                .push(("plugin".into(), name.clone(), factory));
        }
        self.specialists.extend(plugin.specialist_descriptors());
        self
    }

    pub fn build(self) -> Result<GeWuRuntime> {
        let provider = self
            .provider
            .ok_or_else(|| anyhow!("provider is required"))?;
        let pool = self.pool.ok_or_else(|| anyhow!("pool is required"))?;

        let mut registry = CapabilityRegistry::new();
        for (kind, id, factory) in self.extra_providers {
            registry.register(Arc::new(ToolFactoryProvider::new(kind, id, factory)));
        }

        let mut specialists = self.specialists;

        // Load manifest-driven specialists
        if !self.manifest_dirs.is_empty() {
            let mut loader = crate::manifest::loader::SpecialistManifestLoader::new();
            for dir in &self.manifest_dirs {
                loader.add_dir(dir);
            }
            match loader.load(Some(&registry)) {
                Ok(manifest_specs) => {
                    let override_ids: std::collections::HashSet<String> = manifest_specs
                        .iter()
                        .filter(|s| specialists.iter().any(|e| e.id == s.id))
                        .map(|s| {
                            tracing::info!(
                                "Manifest specialist '{}' overrides code-defined descriptor",
                                s.id
                            );
                            s.id.clone()
                        })
                        .collect();
                    if !override_ids.is_empty() {
                        specialists.retain(|s| !override_ids.contains(&s.id));
                    }
                    specialists.extend(manifest_specs);
                }
                Err(e) => {
                    tracing::warn!("Failed to load specialist manifests: {}", e);
                }
            }
        }

        // Register the composer specialist and its tools. The composer
        // context captures a snapshot of the specialists and capabilities
        // that are already registered — so it must come last.
        let composer_desc = composer_specialist_descriptor();
        let composer_ctx = ComposerContext {
            specialists: Arc::new(specialists.clone()),
            capabilities: Arc::new(registry.list_capabilities()),
            workflows: Arc::new(HashMap::new()),
            output_dir: default_workflow_output_dir(),
        };
        let composer_factory = ComposerToolFactory::new(composer_ctx);
        let mut factories = self.factories;
        factories.push(Box::new(composer_factory));
        registry.register(Arc::new(ToolFactoryProvider::new(
            "builtin",
            "composer",
            Box::new(ComposerToolFactoryForRegistry),
        )));
        specialists.push(composer_desc);

        Ok(GeWuRuntime {
            factories,
            capability_registry: Arc::new(registry),
            specialists,
            provider,
            pool,
        })
    }
}

fn default_workflow_output_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".gewu").join("workflows"))
        .unwrap_or_else(|| PathBuf::from(".gewu/workflows"))
}

/// Lightweight factory that registers the composer capability names into the
/// `CapabilityRegistry` index. Actual tool creation is handled by
/// `ComposerToolFactory` (which holds the `ComposerContext`).
struct ComposerToolFactoryForRegistry;

impl ToolFactory for ComposerToolFactoryForRegistry {
    fn capabilities(&self) -> Vec<String> {
        crate::composer::COMPOSER_CAPABILITIES
            .iter()
            .map(|s| s.to_string())
            .collect()
    }
    fn create_tools(
        &self,
        _capability: &str,
        _ctx: &AssemblyContext,
    ) -> std::result::Result<Vec<Box<dyn Tool>>, Box<dyn std::error::Error + Send + Sync>> {
        // The real tools are created by ComposerToolFactory which is in the
        // factories vec. This stub exists only so the registry index knows
        // these capability names exist.
        Ok(vec![])
    }
}
