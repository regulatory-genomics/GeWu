use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use gewu_core::Agent;
use gewu_core::traits::{AssemblyContext, LlmProvider, Plugin, Tool, ToolFactory, ToolRegistry};
use gewu_core::types::{
    AgentEvent, ExecutionRunKind, SpecialistAgentEventRecord, SpecialistDescriptor,
    SpecialistExecutionEnvelope, SpecialistExecutionRequest, SpecialistLlmIoRecord,
    TaskExecutionStatus, TaskResultStatus, TaskStatus, ToolContext, ToolExecutionRecord,
    ToolResult,
};
use gewu_storage::{SqliteMemory, TaskStore};
use gewu_tools::{BatchTool, report_result::ReportResultArgs};
use schemars::schema::RootSchema;
use serde_json::Value;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::capability::{CapabilityRegistry, ToolFactoryProvider};
use crate::command_tool_registry::CommandToolRegistry;
use crate::composer::{ComposerContext, ComposerToolFactory, composer_specialist_descriptor};
use crate::execution_trace::{StartTraceRun, TraceContext, TraceSink, trace_sink_from_pool};
use crate::hooks::{HookRegistry, HttpHookProvider};
use crate::manifest::runtime_plugin_loader::RuntimePluginManifestLoader;
use crate::manifest::schema::RuntimeHookEvent;
use crate::registry::{AgentConfig, BuiltinToolFactory};
use crate::skill_registry::SkillRegistry;
use crate::sop_registry::SopRegistry;
use crate::workflow_registry::WorkflowRegistry;

pub struct GeWuRuntime {
    factories: Vec<Box<dyn ToolFactory>>,
    capability_registry: Arc<CapabilityRegistry>,
    specialists: Vec<SpecialistDescriptor>,
    skill_registry: Arc<SkillRegistry>,
    sop_registry: Arc<SopRegistry>,
    hook_registry: Arc<HookRegistry>,
    provider: Arc<dyn LlmProvider>,
    pool: SqlitePool,
    runtime_plugin_dirs: Vec<PathBuf>,
    skill_dirs: Vec<PathBuf>,
    workflow_dirs: Vec<PathBuf>,
    trace_sink: Arc<dyn TraceSink>,
}

#[derive(Debug, Clone)]
pub enum SpecialistExecutionEvent {
    Agent(AgentEvent),
    Completed(SpecialistExecutionEnvelope),
}

impl GeWuRuntime {
    pub fn builder() -> GeWuRuntimeBuilder {
        GeWuRuntimeBuilder {
            factories: Vec::new(),
            extra_providers: Vec::new(),
            specialists: Vec::new(),
            manifest_dirs: Vec::new(),
            runtime_plugin_dirs: Vec::new(),
            skill_dirs: Vec::new(),
            workflow_dirs: Vec::new(),
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

    pub fn list_specialist_descriptors(&self) -> Vec<SpecialistDescriptor> {
        self.specialists.clone()
    }

    /// Access the runtime's capability registry. Used by manifest-driven paths
    /// and for diagnostics (e.g. `gewu list capabilities`).
    pub fn capability_registry(&self) -> &CapabilityRegistry {
        &self.capability_registry
    }

    pub fn skill_registry(&self) -> &SkillRegistry {
        &self.skill_registry
    }

    pub fn sop_registry(&self) -> &SopRegistry {
        &self.sop_registry
    }

    pub(crate) async fn start_trace_run(
        &self,
        input: StartTraceRun,
    ) -> Result<gewu_core::types::ExecutionRun> {
        self.trace_sink.start_run(input).await
    }

    pub(crate) async fn finish_trace_run(
        &self,
        run_id: &str,
        output_json: Option<&Value>,
    ) -> Result<gewu_core::types::ExecutionRun> {
        self.trace_sink.finish_run(run_id, output_json).await
    }

    pub(crate) async fn fail_trace_run(
        &self,
        run_id: &str,
        error: &str,
    ) -> Result<gewu_core::types::ExecutionRun> {
        self.trace_sink.fail_run(run_id, error).await
    }

    pub(crate) fn descriptor_with_skills(
        &self,
        descriptor: &SpecialistDescriptor,
    ) -> SpecialistDescriptor {
        let mut descriptor = descriptor.clone();
        for skill_id in &descriptor.skills {
            let Some(skill) = self.skill_registry.get(skill_id) else {
                tracing::warn!(
                    "Specialist '{}' references unknown skill '{}'",
                    descriptor.id,
                    skill_id
                );
                continue;
            };
            for capability in &skill.capabilities {
                if !descriptor
                    .capabilities
                    .iter()
                    .any(|existing| existing == capability)
                {
                    descriptor.capabilities.push(capability.clone());
                }
            }
        }
        descriptor
    }

    fn append_skill_prompt(
        &self,
        descriptor: &SpecialistDescriptor,
        system_prompt: String,
    ) -> String {
        if descriptor.skills.is_empty() || self.skill_registry.is_empty() {
            return system_prompt;
        }
        let mut available = Vec::new();
        let mut loaded = Vec::new();
        for skill_id in &descriptor.skills {
            let Some(skill) = self.skill_registry.get(skill_id) else {
                continue;
            };
            available.push(format!("- {}: {}", skill.name, skill.description));
            loaded.push(format!(
                "<skill_content name=\"{}\" location=\"{}\">\n{}\n</skill_content>",
                skill.name,
                skill.location.display(),
                skill.content
            ));
        }
        if available.is_empty() {
            return system_prompt;
        }
        format!(
            "{}\n\n<available_skills>\n{}\n</available_skills>\n\n<loaded_skill_instructions>\n{}\n</loaded_skill_instructions>",
            system_prompt.trim_end(),
            available.join("\n"),
            loaded.join("\n\n")
        )
    }

    fn append_sop_prompt(
        &self,
        descriptor: &SpecialistDescriptor,
        system_prompt: String,
    ) -> String {
        if self.sop_registry.is_empty()
            || !descriptor
                .capabilities
                .iter()
                .any(|cap| cap == "run_workflow")
        {
            return system_prompt;
        }
        let entries = self
            .sop_registry
            .visible_to(&descriptor.id)
            .into_iter()
            .map(|sop| {
                let triggers = if sop.triggers.is_empty() {
                    String::new()
                } else {
                    format!("\n  triggers: {}", sop.triggers.join(", "))
                };
                format!(
                    "- {} ({}) — {}{}",
                    sop.workflow_id, sop.name, sop.description, triggers
                )
            })
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return system_prompt;
        }
        format!(
            "{}\n\n<available_sops>\n{}\n\nIf the user request clearly matches an available SOP, prefer calling run_workflow with the matching workflow_id and a structured input object instead of performing the procedure manually.\n</available_sops>",
            system_prompt.trim_end(),
            entries.join("\n")
        )
    }

    fn apply_prompt_packs(
        &self,
        descriptor: &SpecialistDescriptor,
        system_prompt: String,
    ) -> String {
        let with_runtime_context = append_runtime_context(system_prompt);
        let with_skills = self.append_skill_prompt(descriptor, with_runtime_context);
        self.append_sop_prompt(descriptor, with_skills)
    }

    fn with_runtime_dir_metadata(&self, metadata_json: Value) -> Value {
        let mut metadata = match metadata_json {
            Value::Object(map) => map,
            other => {
                let mut map = serde_json::Map::new();
                map.insert("request_metadata".into(), other);
                map
            }
        };
        metadata.insert(
            "runtime_plugin_dirs".into(),
            Value::Array(
                self.runtime_plugin_dirs
                    .iter()
                    .map(|p| Value::String(p.to_string_lossy().to_string()))
                    .collect(),
            ),
        );
        metadata.insert(
            "skill_dirs".into(),
            Value::Array(
                self.skill_dirs
                    .iter()
                    .map(|p| Value::String(p.to_string_lossy().to_string()))
                    .collect(),
            ),
        );
        metadata.insert(
            "workflow_dirs".into(),
            Value::Array(
                self.workflow_dirs
                    .iter()
                    .map(|p| Value::String(p.to_string_lossy().to_string()))
                    .collect(),
            ),
        );
        Value::Object(metadata)
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
            metadata_json: self.with_runtime_dir_metadata(metadata_json),
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
                // Fall back to capability registry (handles command tools, etc.)
                if self.capability_registry.contains(cap) {
                    let produced = self.capability_registry.resolve(&[cap.clone()], &ctx)?;
                    tools.extend(produced);
                } else {
                    return Err(anyhow!("No factory can resolve capability: {}", cap));
                }
            }
        }
        Ok(tools)
    }

    pub fn resolve_capabilities(&self, caps: &[String]) -> Result<Vec<Box<dyn Tool>>> {
        self.resolve_capabilities_with_working_directory(
            caps,
            std::env::current_dir()?,
            Value::Null,
        )
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

    /// Execute an inline specialist defined in a workflow node.
    /// Unlike execute_request, this doesn't look up the specialist in the registry —
    /// it uses the provided descriptor directly.
    pub async fn execute_inline_specialist(
        &self,
        descriptor: &SpecialistDescriptor,
        prompt: &str,
        session_id: &str,
        tools: Vec<Box<dyn Tool>>,
        _metadata_json: Value,
    ) -> Result<Value> {
        let memory = SqliteMemory::new(self.pool.clone(), session_id.to_string()).await?;

        let descriptor = self.descriptor_with_skills(descriptor);
        let tools_arc: Vec<Arc<dyn Tool>> = tools
            .into_iter()
            .map(|t| Arc::from(t) as Arc<dyn Tool>)
            .collect();

        let tools_arc = self.wrap_tools_with_hooks(
            tools_arc,
            descriptor.id.clone(),
            session_id.to_string(),
            _metadata_json.clone(),
        );

        let tools_for_registry = Arc::new(tools_arc.clone());
        let registry: Arc<dyn ToolRegistry> = Arc::new(RuntimeToolRegistry {
            tools: tools_for_registry,
        });

        let batch_tool: Arc<dyn Tool> = Arc::new(BatchTool::new(registry.clone()));
        let mut all_tools = vec![batch_tool];
        all_tools.extend(tools_arc);

        let system_prompt = if descriptor.system_prompt.is_empty() {
            prompt.to_string()
        } else {
            descriptor.system_prompt.clone()
        };
        let system_prompt = self
            .transform_system_prompt(
                &descriptor,
                self.apply_prompt_packs(&descriptor, system_prompt),
                session_id,
                Some(prompt),
                &_metadata_json,
            )
            .await?;

        let mut agent = Agent::builder()
            .llm(self.provider.clone())
            .with_tools(all_tools)
            .memory(Box::new(memory))
            .system_prompt(&system_prompt)
            .session_id(session_id.to_string())
            .build()?;

        let mut stream = Box::pin(agent.ask(prompt));
        let mut result_text = String::new();

        while let Some(event_result) = futures::StreamExt::next(&mut stream).await {
            match event_result {
                Ok(event) => match event {
                    AgentEvent::Finish { response } => {
                        result_text = response;
                    }
                    AgentEvent::ToolFinish { tool, output, .. } if tool == "report_result" => {
                        if let Ok(report) = serde_json::from_str::<
                            gewu_tools::report_result::ReportResultArgs,
                        >(&output)
                        {
                            if let Some(val) = report.result {
                                return Ok(val);
                            }
                        }
                    }
                    AgentEvent::Error { error } => {
                        return Err(anyhow!("Inline specialist error: {}", error));
                    }
                    _ => {}
                },
                Err(e) => {
                    return Err(anyhow!("Inline specialist stream error: {}", e));
                }
            }
        }

        Ok(Value::String(result_text))
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

        let descriptor = self.descriptor_with_skills(descriptor);
        let tools = self.resolve_capabilities_with_metadata(
            &descriptor.capabilities,
            &std::env::current_dir()?,
            metadata_json.clone(),
        )?;
        let memory = SqliteMemory::new(self.pool.clone(), session_id.clone()).await?;

        let mut tools_arc: Vec<Arc<dyn Tool>> = tools
            .into_iter()
            .map(|t| Arc::from(t) as Arc<dyn Tool>)
            .collect();
        tools_arc.extend(additional_tools);

        let tools_arc = self.wrap_tools_with_hooks(
            tools_arc,
            descriptor.id.clone(),
            session_id.clone(),
            metadata_json.clone(),
        );

        let tools_for_registry = Arc::new(tools_arc.clone());
        let registry: Arc<dyn ToolRegistry> = Arc::new(RuntimeToolRegistry {
            tools: tools_for_registry,
        });

        let batch_tool: Arc<dyn Tool> = Arc::new(BatchTool::new(registry.clone()));
        let mut all_tools = vec![batch_tool];
        all_tools.extend(tools_arc);

        let system_prompt = self
            .transform_system_prompt(
                &descriptor,
                self.apply_prompt_packs(&descriptor, descriptor.system_prompt.clone()),
                &session_id,
                None,
                &metadata_json,
            )
            .await?;

        let agent = Agent::builder()
            .llm(self.provider.clone())
            .with_tools(all_tools)
            .memory(Box::new(memory))
            .system_prompt(&system_prompt)
            .session_id(session_id.clone())
            .build()?;

        Ok((agent, registry))
    }

    pub async fn execute_request(
        &self,
        request: &SpecialistExecutionRequest,
        additional_tools: Vec<Arc<dyn Tool>>,
    ) -> Result<SpecialistExecutionEnvelope> {
        let mut stream = self.execute_request_stream(request.clone(), additional_tools);
        let mut completed = None;
        while let Some(event) = stream.next().await {
            if let SpecialistExecutionEvent::Completed(envelope) = event? {
                completed = Some(envelope);
            }
        }
        completed.ok_or_else(|| anyhow!("specialist execution stream ended without completion"))
    }

    pub fn execute_request_stream(
        &self,
        request: SpecialistExecutionRequest,
        additional_tools: Vec<Arc<dyn Tool>>,
    ) -> BoxStream<'_, Result<SpecialistExecutionEvent>> {
        Box::pin(async_stream::try_stream! {
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
        let descriptor = self.descriptor_with_skills(descriptor);
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

        let tools_arc = self.wrap_tools_with_hooks(
            tools_arc,
            descriptor.id.clone(),
            request.session_id.clone(),
            request.metadata_json.clone(),
        );

        let tools_for_registry = Arc::new(tools_arc.clone());
        let registry: Arc<dyn ToolRegistry> = Arc::new(RuntimeToolRegistry {
            tools: tools_for_registry,
        });

        let batch_tool: Arc<dyn Tool> = Arc::new(BatchTool::new(registry.clone()));
        let mut all_tools = vec![batch_tool];
        all_tools.extend(tools_arc);

        let system_prompt = self
            .transform_system_prompt(
                &descriptor,
                self.apply_prompt_packs(&descriptor, descriptor.system_prompt.clone()),
                &request.session_id,
                Some(&request.prompt),
                &request.metadata_json,
            )
            .await?;

        let specialist_run = self.trace_sink.start_run(StartTraceRun {
            parent_id: request
                .metadata_json
                .get("trace_current_run_id")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            session_id: Some(request.session_id.clone()),
            kind: ExecutionRunKind::Specialist,
            name: request.specialist_id.clone(),
            input_json: Some(serde_json::json!({
                "task_id": request.task_id,
                "specialist_id": request.specialist_id,
                "prompt": request.prompt,
                "metadata": request.metadata_json,
            })),
        }).await?;
        let parent_session_id = request
            .metadata_json
            .get("parent_session_id")
            .and_then(Value::as_str);
        store
            .mark_started(
                &request.task_id,
                parent_session_id,
                Some(&request.session_id),
                Some(&specialist_run.id),
            )
            .await?;
        let trace_context = TraceContext {
            root_run_id: request
                .metadata_json
                .get("trace_root_run_id")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .or_else(|| Some(specialist_run.id.clone())),
            current_run_id: Some(specialist_run.id.clone()),
            session_id: request.session_id.clone(),
        };

        let mut agent = Agent::builder()
            .llm(self.provider.clone())
            .with_tools(all_tools)
            .memory(Box::new(memory))
            .system_prompt(&system_prompt)
            .session_id(request.session_id.clone())
            .tool_context_metadata(trace_context.to_metadata_map())
            .build()?;

        store
            .update_status(&request.task_id, &TaskStatus::Running, None)
            .await?;
        store.increment_attempt(&request.task_id).await?;

        self.emit_user_prompt_hook(&descriptor, &request).await;

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
                        let record = SpecialistAgentEventRecord {
                            event_type: "content_delta".to_string(),
                            tool_name: None,
                            payload_json: serde_json::json!({ "delta": delta.clone() }),
                        };
                        self.emit_agent_event_hook(&descriptor, &request, &record)
                            .await;
                        events.push(record);
                        result_text.push_str(&delta);
                        yield SpecialistExecutionEvent::Agent(AgentEvent::ContentDelta { delta });
                    }
                    AgentEvent::ReasoningDelta { delta } => {
                        let record = SpecialistAgentEventRecord {
                            event_type: "reasoning_delta".to_string(),
                            tool_name: None,
                            payload_json: serde_json::json!({ "delta": delta.clone() }),
                        };
                        self.emit_agent_event_hook(&descriptor, &request, &record)
                            .await;
                        events.push(record);
                        yield SpecialistExecutionEvent::Agent(AgentEvent::ReasoningDelta { delta });
                    }
                    AgentEvent::ToolStart { tool_call_id, tool, input, metadata } => {
                        let record = SpecialistAgentEventRecord {
                            event_type: "tool_start".to_string(),
                            tool_name: Some(tool.clone()),
                            payload_json: serde_json::json!({ "tool_call_id": tool_call_id.clone(), "input": input.clone(), "metadata": metadata.clone() }),
                        };
                        self.emit_agent_event_hook(&descriptor, &request, &record)
                            .await;
                        events.push(record);
                        yield SpecialistExecutionEvent::Agent(AgentEvent::ToolStart { tool_call_id, tool, input, metadata });
                    }
                    AgentEvent::Finish { response } => {
                        let record = SpecialistAgentEventRecord {
                            event_type: "finish".to_string(),
                            tool_name: None,
                            payload_json: serde_json::json!({ "response": response.clone() }),
                        };
                        self.emit_agent_event_hook(&descriptor, &request, &record)
                            .await;
                        events.push(record);
                        if !report_received {
                            result_text = response.clone();
                        }
                        yield SpecialistExecutionEvent::Agent(AgentEvent::Finish { response });
                    }
                    AgentEvent::ToolFinish { tool_call_id, tool, output } if tool == "report_result" => {
                        let record = SpecialistAgentEventRecord {
                            event_type: "tool_finish".to_string(),
                            tool_name: Some(tool.clone()),
                            payload_json: serde_json::json!({ "tool_call_id": tool_call_id.clone(), "output": output.clone() }),
                        };
                        self.emit_agent_event_hook(&descriptor, &request, &record)
                            .await;
                        events.push(record);
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
                        yield SpecialistExecutionEvent::Agent(AgentEvent::ToolFinish { tool_call_id, tool, output });
                    }
                    AgentEvent::ToolFinish { tool_call_id, tool, output } => {
                        let record = SpecialistAgentEventRecord {
                            event_type: "tool_finish".to_string(),
                            tool_name: Some(tool.clone()),
                            payload_json: serde_json::json!({ "tool_call_id": tool_call_id.clone(), "output": output.clone() }),
                        };
                        self.emit_agent_event_hook(&descriptor, &request, &record)
                            .await;
                        events.push(record);
                        tool_outputs.push(ToolExecutionRecord {
                            tool_name: tool.clone(),
                            output: output.clone(),
                        });
                        yield SpecialistExecutionEvent::Agent(AgentEvent::ToolFinish { tool_call_id, tool, output });
                    }
                    AgentEvent::Error { error } => {
                        let record = SpecialistAgentEventRecord {
                            event_type: "error".to_string(),
                            tool_name: None,
                            payload_json: serde_json::json!({ "error": error.clone() }),
                        };
                        self.emit_agent_event_hook(&descriptor, &request, &record)
                            .await;
                        events.push(record);
                        final_status = TaskStatus::Blocked;
                        block_reason = Some(error.clone());
                        final_error = Some(error.clone());
                        result_text = error.clone();
                        output_json = None;
                        yield SpecialistExecutionEvent::Agent(AgentEvent::Error { error });
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

        if let Some(error) = &final_error {
            if let Err(err) = self.trace_sink.fail_run(&specialist_run.id, error).await {
                tracing::warn!("failed to mark specialist trace run failed: {}", err);
            }
        } else if final_status == TaskStatus::Blocked {
            if let Err(err) = self
                .trace_sink
                .fail_run(
                    &specialist_run.id,
                    block_reason.as_deref().unwrap_or("specialist blocked"),
                )
                .await
            {
                tracing::warn!("failed to mark specialist trace run blocked: {}", err);
            }
        } else {
            let trace_output = output_json
                .clone()
                .unwrap_or_else(|| Value::String(result_text.clone()));
            if let Err(err) = self
                .trace_sink
                .finish_run(&specialist_run.id, Some(&trace_output))
                .await
            {
                tracing::warn!("failed to finish specialist trace run: {}", err);
            }
        }

        let execution_status = match final_status {
            TaskStatus::Succeeded | TaskStatus::Blocked => TaskExecutionStatus::Completed,
            TaskStatus::Failed => TaskExecutionStatus::Failed,
            TaskStatus::Ready | TaskStatus::Running => TaskExecutionStatus::Interrupted,
        };
        let result_status = classify_task_result(final_status.clone(), &result_text, &output_json);
        let finish_reason = match execution_status {
            TaskExecutionStatus::Completed => Some("stop"),
            TaskExecutionStatus::Failed => Some("error"),
            TaskExecutionStatus::Canceled => Some("canceled"),
            TaskExecutionStatus::Timeout => Some("timeout"),
            TaskExecutionStatus::Interrupted => Some("interrupted"),
            TaskExecutionStatus::Pending | TaskExecutionStatus::Running => None,
        };
        store
            .mark_finished(
                &request.task_id,
                &execution_status,
                &result_status,
                finish_reason,
                final_error.as_deref().or(block_reason.as_deref()),
                Some(&specialist_run.id),
            )
            .await?;

        let attempt = store
            .get_task(&request.task_id)
            .await?
            .map(|r| r.attempt)
            .unwrap_or(0);

        yield SpecialistExecutionEvent::Completed(SpecialistExecutionEnvelope {
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
                system_prompt,
                user_prompt: request.prompt.clone(),
                final_response: Some(result_text),
                error: final_error,
            }],
        });
        })
    }

    async fn transform_system_prompt(
        &self,
        descriptor: &SpecialistDescriptor,
        system_prompt: String,
        session_id: &str,
        user_prompt: Option<&str>,
        metadata_json: &Value,
    ) -> Result<String> {
        if self.hook_registry.is_empty() {
            return Ok(system_prompt);
        }
        let payload = serde_json::json!({
            "event": RuntimeHookEvent::SpecialistSystemPromptTransform.as_str(),
            "user_id": metadata_json.get("user_id").and_then(Value::as_str).unwrap_or("default"),
            "project_id": metadata_json.get("project_id").and_then(Value::as_str),
            "agent_id": descriptor.id.clone(),
            "specialist_id": descriptor.id.clone(),
            "session_id": session_id,
            "workspace_root": std::env::current_dir().ok().map(|p| p.display().to_string()),
            "prompt": user_prompt.unwrap_or_default(),
            "system_prompt": system_prompt.clone(),
            "metadata": {
                "host": "gewu",
                "runtime_metadata": metadata_json,
            },
        });
        self.hook_registry
            .transform_system_prompt(system_prompt, payload)
            .await
    }

    fn wrap_tools_with_hooks(
        &self,
        tools: Vec<Arc<dyn Tool>>,
        specialist_id: String,
        session_id: String,
        metadata_json: Value,
    ) -> Vec<Arc<dyn Tool>> {
        tools
            .into_iter()
            .map(|tool| {
                Arc::new(HookedTool {
                    inner: tool,
                    hook_registry: self.hook_registry.clone(),
                    trace_sink: self.trace_sink.clone(),
                    specialist_id: specialist_id.clone(),
                    session_id: session_id.clone(),
                    metadata_json: metadata_json.clone(),
                }) as Arc<dyn Tool>
            })
            .collect()
    }

    async fn emit_agent_event_hook(
        &self,
        descriptor: &SpecialistDescriptor,
        request: &SpecialistExecutionRequest,
        record: &SpecialistAgentEventRecord,
    ) {
        if self.hook_registry.is_empty() {
            return;
        }
        let content = record
            .payload_json
            .get("delta")
            .or_else(|| record.payload_json.get("response"))
            .or_else(|| record.payload_json.get("input"))
            .or_else(|| record.payload_json.get("output"))
            .or_else(|| record.payload_json.get("error"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| record.payload_json.to_string());
        let event_kind = match record.event_type.as_str() {
            "content_delta" | "finish" => "assistant_response",
            "tool_start" => "tool_call",
            "tool_finish" => "tool_result",
            "error" => "system_note",
            _ => "system_note",
        };
        let payload = serde_json::json!({
            "event": RuntimeHookEvent::AgentEvent.as_str(),
            "event_kind": event_kind,
            "user_id": request.metadata_json.get("user_id").and_then(Value::as_str).unwrap_or("default"),
            "project_id": request.metadata_json.get("project_id").and_then(Value::as_str),
            "workspace_root": std::env::current_dir().ok().map(|p| p.display().to_string()),
            "agent_id": descriptor.id.clone(),
            "specialist_id": descriptor.id.clone(),
            "session_id": request.session_id.clone(),
            "host": "gewu",
            "role": if event_kind == "assistant_response" { "assistant" } else if event_kind == "tool_result" { "tool" } else { "system" },
            "layer": "raw",
            "agent_event_type": record.event_type.clone(),
            "tool_name": record.tool_name.clone(),
            "content": content,
            "content_json": record.payload_json.clone(),
            "metadata": {
                "host": "gewu",
                "hook_event": RuntimeHookEvent::AgentEvent.as_str(),
                "agent_event_type": record.event_type.clone(),
                "tool_name": record.tool_name.clone(),
                "payload": record.payload_json.clone(),
            },
        });
        if let Err(err) = self
            .hook_registry
            .call_event(RuntimeHookEvent::AgentEvent, payload)
            .await
        {
            tracing::warn!("agent.event hook failed: {}", err);
        }
    }

    async fn emit_user_prompt_hook(
        &self,
        descriptor: &SpecialistDescriptor,
        request: &SpecialistExecutionRequest,
    ) {
        if self.hook_registry.is_empty() {
            return;
        }
        let payload = serde_json::json!({
            "event": RuntimeHookEvent::AgentEvent.as_str(),
            "event_kind": "user_message",
            "user_id": request.metadata_json.get("user_id").and_then(Value::as_str).unwrap_or("default"),
            "project_id": request.metadata_json.get("project_id").and_then(Value::as_str),
            "workspace_root": std::env::current_dir().ok().map(|p| p.display().to_string()),
            "agent_id": descriptor.id.clone(),
            "specialist_id": descriptor.id.clone(),
            "session_id": request.session_id.clone(),
            "host": "gewu",
            "role": "user",
            "layer": "raw",
            "agent_event_type": "user_prompt",
            "content": request.prompt.clone(),
            "content_json": {
                "prompt": request.prompt.clone(),
                "task_id": request.task_id.clone(),
                "metadata": request.metadata_json.clone(),
            },
            "metadata": {
                "host": "gewu",
                "hook_event": RuntimeHookEvent::AgentEvent.as_str(),
                "agent_event_type": "user_prompt",
                "payload": {
                    "task_id": request.task_id.clone(),
                    "metadata": request.metadata_json.clone(),
                },
            },
        });
        if let Err(err) = self
            .hook_registry
            .call_event(RuntimeHookEvent::AgentEvent, payload)
            .await
        {
            tracing::warn!("agent.event user prompt hook failed: {}", err);
        }
    }
}

struct HookedTool {
    inner: Arc<dyn Tool>,
    hook_registry: Arc<HookRegistry>,
    trace_sink: Arc<dyn TraceSink>,
    specialist_id: String,
    session_id: String,
    metadata_json: Value,
}

#[async_trait]
impl Tool for HookedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn schema(&self) -> RootSchema {
        self.inner.schema()
    }

    fn execution_policy(&self) -> gewu_core::types::ToolExecutionPolicy {
        self.inner.execution_policy()
    }

    fn start_metadata(
        &self,
        args: &str,
        ctx: &ToolContext,
    ) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        self.inner.start_metadata(args, ctx)
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> std::result::Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let parent_id = ctx
            .metadata
            .get("trace_current_run_id")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let run = match self
            .trace_sink
            .start_run(StartTraceRun {
                parent_id,
                session_id: Some(ctx.session_id.clone()),
                kind: ExecutionRunKind::Tool,
                name: self.inner.name().to_string(),
                input_json: Some(args.clone()),
            })
            .await
        {
            Ok(run) => Some(run),
            Err(err) => {
                tracing::warn!("failed to start tool trace run: {}", err);
                None
            }
        };

        if !self.hook_registry.is_empty() {
            let before_payload = self.tool_hook_payload(
                RuntimeHookEvent::ToolExecuteBefore,
                "tool_call",
                args.to_string(),
                serde_json::json!({ "args": args.clone() }),
                ctx,
            );
            if let Err(err) = self
                .hook_registry
                .call_event(RuntimeHookEvent::ToolExecuteBefore, before_payload)
                .await
            {
                tracing::warn!("tool.execute.before hook failed: {}", err);
            }
        }

        let result = self.inner.execute(args.clone(), ctx).await;

        let (content, metadata) = match &result {
            Ok(output) => (
                output.output.clone(),
                serde_json::json!({ "args": args, "status": "ok" }),
            ),
            Err(err) => (
                err.to_string(),
                serde_json::json!({ "args": args, "status": "error" }),
            ),
        };
        if let Some(run) = &run {
            let finish = match &result {
                Ok(output) => self
                    .trace_sink
                    .finish_run(&run.id, Some(&serde_json::json!({
                        "title": output.title,
                        "output": output.output,
                        "truncated": output.truncated,
                        "output_path": output.output_path.as_ref().map(|p| p.display().to_string()),
                    })))
                    .await,
                Err(err) => self.trace_sink.fail_run(&run.id, &err.to_string()).await,
            };
            if let Err(err) = finish {
                tracing::warn!("failed to finish tool trace run: {}", err);
            }
        }

        if !self.hook_registry.is_empty() {
            let after_payload = self.tool_hook_payload(
                RuntimeHookEvent::ToolExecuteAfter,
                "tool_result",
                content,
                metadata,
                ctx,
            );
            if let Err(err) = self
                .hook_registry
                .call_event(RuntimeHookEvent::ToolExecuteAfter, after_payload)
                .await
            {
                tracing::warn!("tool.execute.after hook failed: {}", err);
            }
        }

        result
    }
}

impl HookedTool {
    fn tool_hook_payload(
        &self,
        event: RuntimeHookEvent,
        event_kind: &str,
        content: String,
        metadata: Value,
        ctx: &ToolContext,
    ) -> Value {
        serde_json::json!({
            "event": event.as_str(),
            "event_kind": event_kind,
            "user_id": self.metadata_json.get("user_id").and_then(Value::as_str).unwrap_or("default"),
            "project_id": self.metadata_json.get("project_id").and_then(Value::as_str),
            "workspace_root": std::env::current_dir().ok().map(|p| p.display().to_string()),
            "agent_id": self.specialist_id.clone(),
            "specialist_id": self.specialist_id.clone(),
            "session_id": self.session_id.clone(),
            "host": "gewu",
            "role": if event_kind == "tool_result" { "tool" } else { "assistant" },
            "layer": "raw",
            "tool_name": self.inner.name(),
            "content": content,
            "content_json": metadata.clone(),
            "metadata": {
                "host": "gewu",
                "hook_event": event.as_str(),
                "tool_name": self.inner.name(),
                "tool_context_session_id": ctx.session_id.clone(),
                "tool_context_message_id": ctx.message_id.clone(),
                "payload": metadata,
            },
        })
    }
}

fn append_runtime_context(system_prompt: String) -> String {
    let now = chrono::Local::now();
    format!(
        "{}\n\n<runtime_context>\nCurrent date: {}\nCurrent weekday: {}\nTimezone: local server timezone\n</runtime_context>",
        system_prompt.trim_end(),
        now.format("%Y-%m-%d"),
        now.format("%A")
    )
}

fn classify_task_result(
    final_status: TaskStatus,
    result_text: &str,
    output_json: &Option<Value>,
) -> TaskResultStatus {
    if final_status == TaskStatus::Blocked {
        return TaskResultStatus::Invalid;
    }
    if final_status != TaskStatus::Succeeded {
        return TaskResultStatus::Invalid;
    }
    if output_json.is_some() {
        return TaskResultStatus::Accepted;
    }
    let normalized = result_text.trim().to_lowercase();
    if normalized.is_empty() {
        return TaskResultStatus::Empty;
    }
    let future_work_markers = [
        "让我生成",
        "我将生成",
        "接下来我会",
        "稍后",
        "let me generate",
        "i will generate",
        "i will now",
        "next i will",
    ];
    if future_work_markers
        .iter()
        .any(|marker| normalized.contains(marker))
    {
        return TaskResultStatus::Incomplete;
    }
    TaskResultStatus::Accepted
}

#[cfg(test)]
mod task_result_tests {
    use super::*;

    #[test]
    fn blocked_specialist_is_completed_execution_with_invalid_result() {
        let final_status = TaskStatus::Blocked;
        let execution_status = match final_status {
            TaskStatus::Succeeded | TaskStatus::Blocked => TaskExecutionStatus::Completed,
            TaskStatus::Failed => TaskExecutionStatus::Failed,
            TaskStatus::Ready | TaskStatus::Running => TaskExecutionStatus::Interrupted,
        };

        assert_eq!(execution_status, TaskExecutionStatus::Completed);
        assert_eq!(
            classify_task_result(
                final_status,
                "API error: invalid params, tool result's tool id not found",
                &None,
            ),
            TaskResultStatus::Invalid
        );
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
    runtime_plugin_dirs: Vec<PathBuf>,
    skill_dirs: Vec<PathBuf>,
    workflow_dirs: Vec<PathBuf>,
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
        self.extra_providers.push((
            "builtin".into(),
            "builtin".into(),
            Box::new(BuiltinToolFactory),
        ));
        self
    }

    pub fn register_builtin_specialists(mut self) -> Self {
        self.specialists
            .extend(crate::registry::specialist_descriptors());
        self
    }

    pub fn with_manifest_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.manifest_dirs.push(dir.into());
        self
    }

    pub fn with_runtime_plugin_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.runtime_plugin_dirs.push(dir.into());
        self
    }

    pub fn with_runtime_plugin_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.runtime_plugin_dirs.extend(dirs);
        self
    }

    pub fn with_skill_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.skill_dirs.push(dir.into());
        self
    }

    pub fn with_skill_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.skill_dirs.extend(dirs);
        self
    }

    pub fn with_workflow_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.workflow_dirs.push(dir.into());
        self
    }

    pub fn with_workflow_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.workflow_dirs.extend(dirs);
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
        let skill_registry = Arc::new(SkillRegistry::from_dirs(&self.skill_dirs)?);
        let workflow_registry = WorkflowRegistry::from_dirs(&self.workflow_dirs)?;
        let sop_registry = Arc::new(SopRegistry::from_workflows(workflow_registry.as_map()));
        let mut hook_registry = HookRegistry::new();

        // Load command-backed tools from ~/.gewu/tools/
        let tools_dir = CommandToolRegistry::default_dir();
        match CommandToolRegistry::new(&tools_dir) {
            Ok(cmd_registry) => {
                let count = cmd_registry.definitions().len();
                if count > 0 {
                    tracing::info!(
                        "Loaded {} command tool(s) from {}",
                        count,
                        tools_dir.display()
                    );
                    registry.register(Arc::new(cmd_registry.into_provider()));
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to load command tools from {}: {}",
                    tools_dir.display(),
                    e
                );
            }
        }

        // Load HTTP tool providers from ~/.gewu/tool-providers/
        let tool_providers_dir = dirs::home_dir()
            .map(|h| h.join(".gewu").join("tool-providers"))
            .unwrap_or_else(|| PathBuf::from(".gewu/tool-providers"));

        if tool_providers_dir.is_dir() {
            match std::fs::read_dir(&tool_providers_dir) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let is_yaml = path
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e == "yaml" || e == "yml")
                            .unwrap_or(false);
                        if !is_yaml {
                            continue;
                        }

                        match std::fs::read_to_string(&path) {
                            Ok(content) => {
                                match serde_yaml::from_str::<
                                    crate::manifest::schema::ToolProviderManifest,
                                >(&content)
                                {
                                    Ok(manifest) if manifest.kind == "http" => {
                                        match serde_yaml::from_str::<
                                            crate::providers::http::HttpProviderSpec,
                                        >(&content)
                                        {
                                            Ok(http_spec) => {
                                                let provider_id = http_spec.id.clone();
                                                let cap_count = http_spec.capabilities.len();
                                                registry.register(Arc::new(
                                                    crate::providers::http::HttpToolProvider::from_spec(http_spec),
                                                ));
                                                tracing::info!(
                                                    "Loaded HTTP tool provider '{}' with {} capabilities from {}",
                                                    provider_id,
                                                    cap_count,
                                                    path.display()
                                                );
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    "Failed to parse HTTP provider spec from {}: {}",
                                                    path.display(),
                                                    e
                                                );
                                            }
                                        }
                                    }
                                    Ok(_) => {
                                        tracing::debug!(
                                            "Skipping non-HTTP tool provider: {}",
                                            path.display()
                                        );
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "Failed to parse tool provider manifest {}: {}",
                                            path.display(),
                                            e
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to read {}: {}", path.display(), e);
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        "No tool-providers directory at {}: {}",
                        tool_providers_dir.display(),
                        e
                    );
                }
            }
        }

        let mut runtime_plugin_loader = RuntimePluginManifestLoader::with_default_dirs();
        for dir in &self.runtime_plugin_dirs {
            runtime_plugin_loader.add_dir(dir);
        }
        match runtime_plugin_loader.load() {
            Ok(runtime_plugins) => {
                for loaded in runtime_plugins {
                    match loaded.manifest.kind {
                        crate::manifest::schema::RuntimePluginKind::Http => {
                            match crate::providers::http::HttpToolProvider::from_runtime_plugin(
                                &loaded.manifest,
                            ) {
                                Ok(Some(provider)) => {
                                    tracing::info!(
                                        "Loaded runtime plugin HTTP tools '{}' from {}",
                                        loaded.manifest.id,
                                        loaded.source_path.display()
                                    );
                                    registry.register(Arc::new(provider));
                                }
                                Ok(None) => {}
                                Err(e) => tracing::warn!(
                                    "Failed to load runtime plugin tools from {}: {}",
                                    loaded.source_path.display(),
                                    e
                                ),
                            }
                            match HttpHookProvider::from_runtime_plugin(&loaded.manifest) {
                                Ok(Some(provider)) => {
                                    tracing::info!(
                                        "Loaded runtime plugin hooks '{}' from {}",
                                        provider.id(),
                                        loaded.source_path.display()
                                    );
                                    hook_registry.register(provider);
                                }
                                Ok(None) => {}
                                Err(e) => tracing::warn!(
                                    "Failed to load runtime plugin hooks from {}: {}",
                                    loaded.source_path.display(),
                                    e
                                ),
                            }
                        }
                    }
                }
            }
            Err(e) => tracing::warn!("Failed to load runtime plugin manifests: {}", e),
        }

        // Load manifest-driven specialists (after tools so capability validation works)
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

        let trace_sink = trace_sink_from_pool(pool.clone());

        Ok(GeWuRuntime {
            factories,
            capability_registry: Arc::new(registry),
            specialists,
            skill_registry,
            sop_registry,
            hook_registry: Arc::new(hook_registry),
            provider,
            pool,
            runtime_plugin_dirs: self.runtime_plugin_dirs,
            skill_dirs: self.skill_dirs,
            workflow_dirs: self.workflow_dirs,
            trace_sink,
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
