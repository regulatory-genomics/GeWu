use anyhow::{Result, anyhow};
use futures::{StreamExt, stream::BoxStream};
use gewu_core::Agent;
use gewu_core::traits::{AssemblyContext, LlmProvider, Plugin, Tool, ToolFactory, ToolRegistry};
use gewu_core::types::{
    AgentEvent, ExecutionRunKind, SpecialistAgentEventRecord, SpecialistDescriptor,
    SpecialistExecutionEnvelope, SpecialistExecutionRequest, SpecialistLlmIoRecord,
    TaskExecutionStatus, TaskResultStatus, TaskStatus, ToolExecutionRecord,
};
use gewu_resource_core::{
    CatalogLoader, MutableResourceProvider, ResourceRegistry, ResourceToolFactory,
};
use gewu_storage::{SqliteMemory, TaskStore};
use gewu_tools::{BatchTool, report_result::ReportResultArgs};
use serde_json::Value;
use sqlx::SqlitePool;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::capability::{CapabilityRegistry, ToolFactoryProvider};
use crate::command_tool_registry::CommandToolRegistry;
use crate::composer::{ComposerContext, ComposerToolFactory, composer_specialist_descriptor};
use crate::execution_trace::{StartTraceRun, TraceContext, TraceSink, trace_sink_from_pool};
use crate::hooked_tool::HookedTool;
use crate::hooks::{HookRegistry, HttpHookProvider};
use crate::manifest::runtime_plugin_loader::RuntimePluginManifestLoader;
use crate::manifest::schema::RuntimeHookEvent;
use crate::registry::{AgentConfig, BuiltinToolFactory};
use crate::runtime_tool_registry::RuntimeToolRegistry;
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
            resource_dirs: Vec::new(),
            command_tool_dirs: Vec::new(),
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
        self.warn_unknown_skills(&descriptor);
        for capability in self
            .skill_registry
            .capabilities_for_names(&descriptor.skills)
        {
            if !descriptor
                .capabilities
                .iter()
                .any(|existing| existing == &capability)
            {
                descriptor.capabilities.push(capability);
            }
        }
        descriptor
    }

    pub fn skill_summaries(&self) -> Vec<crate::skill_registry::SkillSummary> {
        self.skill_registry.summaries()
    }

    fn known_skill_names_for_descriptor(&self, descriptor: &SpecialistDescriptor) -> Vec<String> {
        let mut seen = BTreeSet::new();
        descriptor
            .skills
            .iter()
            .filter(|name| self.skill_registry.get(name).is_some())
            .filter(|name| seen.insert((*name).clone()))
            .cloned()
            .collect()
    }

    fn warn_unknown_skills(&self, descriptor: &SpecialistDescriptor) {
        for skill_id in &descriptor.skills {
            if self.skill_registry.get(skill_id).is_none() {
                tracing::warn!(
                    "Specialist '{}' references unknown skill '{}'",
                    descriptor.id,
                    skill_id
                );
            }
        }
    }

    fn skill_tools(&self, descriptor: &SpecialistDescriptor) -> Vec<Arc<dyn Tool>> {
        let allowed_names = self.known_skill_names_for_descriptor(descriptor);
        if allowed_names.is_empty() {
            return Vec::new();
        }
        vec![Arc::new(crate::skill_tools::LoadSkillTool::new_scoped(
            self.skill_registry.clone(),
            allowed_names,
        ))]
    }

    fn append_skill_prompt(
        &self,
        descriptor: &SpecialistDescriptor,
        system_prompt: String,
    ) -> String {
        if self.skill_registry.is_empty() {
            return system_prompt;
        }
        self.warn_unknown_skills(descriptor);
        let available = self
            .skill_registry
            .summaries_for_names(&descriptor.skills)
            .into_iter()
            .map(|skill| format!("- {}: {}", skill.name, skill.description))
            .collect::<Vec<_>>();
        if available.is_empty() {
            return system_prompt;
        }
        format!(
            "{}\n\n<available_skills>\n{}\n\nUse the `load_skill` tool before following any skill's full instructions.\n</available_skills>",
            system_prompt.trim_end(),
            available.join("\n")
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
                        .map_err(|e| anyhow!("Factory failed for '{cap}': {e}"))?;
                    tools.extend(produced);
                    resolved = true;
                    break;
                }
            }
            if !resolved {
                // Fall back to capability registry (handles command tools, etc.)
                if self.capability_registry.contains(cap) {
                    let produced = self
                        .capability_registry
                        .resolve(std::slice::from_ref(cap), &ctx)?;
                    tools.extend(produced);
                } else {
                    return Err(anyhow!("No factory can resolve capability: {cap}"));
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
        working_directory: &Path,
    ) -> Result<Vec<Box<dyn Tool>>> {
        self.resolve_capabilities_with_working_directory(
            caps,
            working_directory.to_path_buf(),
            Value::Null,
        )
    }

    pub fn resolve_capabilities_with_metadata(
        &self,
        caps: &[String],
        working_directory: &Path,
        metadata_json: Value,
    ) -> Result<Vec<Box<dyn Tool>>> {
        self.resolve_capabilities_with_working_directory(
            caps,
            working_directory.to_path_buf(),
            metadata_json,
        )
    }

    /// Execute an inline specialist defined in a workflow node.
    /// Unlike execute_request, this doesn't look up the specialist in the registry —
    /// it uses the provided descriptor directly.

    fn canvas_observation_source_for(
        &self,
        descriptor: &SpecialistDescriptor,
    ) -> Option<Arc<dyn gewu_core::observation::ObservationSource>> {
        let needs_canvas_observations = descriptor.id == "canvas-analyst"
            || descriptor
                .capabilities
                .iter()
                .any(|capability| capability == "workflow_emit");
        if !needs_canvas_observations {
            return None;
        }
        let url = std::env::var("GEWU_TACHYON_CANVAS_URL")
            .unwrap_or_else(|_| "http://localhost:3210".to_string());
        Some(Arc::new(
            gewu_canvas::observation::TachyonObservationSource::new(&url),
        ))
    }

    pub async fn execute_inline_specialist(
        &self,
        descriptor: &SpecialistDescriptor,
        prompt: &str,
        session_id: &str,
        tools: Vec<Box<dyn Tool>>,
        metadata_json: Value,
    ) -> Result<Value> {
        let memory = SqliteMemory::new(self.pool.clone(), session_id.to_string()).await?;

        let descriptor = self.descriptor_with_skills(descriptor);
        let tools_arc: Vec<Arc<dyn Tool>> = tools
            .into_iter()
            .map(|t| Arc::from(t) as Arc<dyn Tool>)
            .collect();
        let mut tools_arc = tools_arc;
        tools_arc.extend(self.skill_tools(&descriptor));

        let tools_arc = self.wrap_tools_with_hooks(
            tools_arc,
            descriptor.id.clone(),
            session_id.to_string(),
            metadata_json.clone(),
        );

        let tools_for_registry = Arc::new(tools_arc.clone());
        let registry: Arc<dyn ToolRegistry> =
            Arc::new(RuntimeToolRegistry::new(tools_for_registry));

        let batch_tool: Arc<dyn Tool> = Arc::new(BatchTool::new(registry.clone()));
        let mut all_tools = vec![batch_tool];
        all_tools.extend(tools_arc);

        let system_prompt = self
            .transform_system_prompt(
                &descriptor,
                self.apply_prompt_packs(&descriptor, descriptor.system_prompt.clone()),
                session_id,
                None,
                &metadata_json,
            )
            .await?;

        let mut agent_builder = Agent::builder()
            .llm(self.provider.clone())
            .with_tools(all_tools)
            .memory(Box::new(memory))
            .system_prompt(&system_prompt)
            .session_id(session_id);
        if let Some(source) = self.canvas_observation_source_for(&descriptor) {
            agent_builder = agent_builder.observation_source(source);
        }
        let mut agent = agent_builder.build()?;

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
                            && let Some(val) = report.result
                        {
                            return Ok(val);
                        }
                    }
                    AgentEvent::Error { error } => {
                        return Err(anyhow!("Inline specialist error: {error}"));
                    }
                    _ => {}
                },
                Err(e) => {
                    return Err(anyhow!("Inline specialist stream error: {e}"));
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
        let working_directory = working_directory_from_metadata(&metadata_json)?;
        self.build_specialist_with_metadata_in_directory(
            name,
            session_id,
            additional_tools,
            metadata_json,
            working_directory,
        )
        .await
    }

    pub async fn build_specialist_with_metadata_in_directory(
        &self,
        name: &str,
        session_id: String,
        additional_tools: Vec<Arc<dyn Tool>>,
        metadata_json: Value,
        working_directory: PathBuf,
    ) -> Result<(Agent, Arc<dyn ToolRegistry>)> {
        let descriptor = self
            .find_specialist(name)
            .ok_or_else(|| anyhow!("Unknown specialist: {name}"))?;

        let descriptor = self.descriptor_with_skills(descriptor);
        let metadata_json = metadata_with_workspace_root(metadata_json, &working_directory);
        let tools = self.resolve_capabilities_with_metadata(
            &descriptor.capabilities,
            &working_directory,
            metadata_json.clone(),
        )?;
        let memory = SqliteMemory::new(self.pool.clone(), session_id.clone()).await?;

        let mut tools_arc: Vec<Arc<dyn Tool>> = tools
            .into_iter()
            .map(|t| Arc::from(t) as Arc<dyn Tool>)
            .collect();
        append_tools_overriding_existing(&mut tools_arc, additional_tools);
        tools_arc.extend(self.skill_tools(&descriptor));

        let tools_arc = self.wrap_tools_with_hooks(
            tools_arc,
            descriptor.id.clone(),
            session_id.clone(),
            metadata_json.clone(),
        );

        let registry = RuntimeToolRegistry::new(Arc::new(tools_arc.clone()));

        let batch_tool: Arc<dyn Tool> = Arc::new(BatchTool::new(Arc::new(registry.clone())));
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

        let mut agent_builder = Agent::builder()
            .llm(self.provider.clone())
            .with_tools(all_tools)
            .memory(Box::new(memory))
            .system_prompt(&system_prompt)
            .session_id(session_id.clone())
            .tool_context_metadata(metadata_object_for_tool_context(&metadata_json));
        if let Some(source) = self.canvas_observation_source_for(&descriptor) {
            agent_builder = agent_builder.observation_source(source);
        }
        let agent = agent_builder.build()?;

        Ok((agent, Arc::new(registry)))
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
        let working_directory = request_working_directory(&request);
        self.execute_request_stream_in_directory(request, additional_tools, working_directory)
    }

    pub fn execute_request_stream_in_directory(
        &self,
        request: SpecialistExecutionRequest,
        additional_tools: Vec<Arc<dyn Tool>>,
        working_directory: Result<PathBuf>,
    ) -> BoxStream<'_, Result<SpecialistExecutionEvent>> {
        Box::pin(async_stream::try_stream! {
        let mut request = request;
        let working_directory = working_directory?;
        request.metadata_json = metadata_with_workspace_root(request.metadata_json.clone(), &working_directory);
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
            &working_directory,
            request.metadata_json.clone(),
        )?;
        let memory = SqliteMemory::new(self.pool.clone(), request.session_id.clone()).await?;

        let mut tools_arc: Vec<Arc<dyn Tool>> = tools
            .into_iter()
            .map(|t| Arc::from(t) as Arc<dyn Tool>)
            .collect();
        append_tools_overriding_existing(&mut tools_arc, additional_tools);
        tools_arc.extend(self.skill_tools(&descriptor));

        let tools_arc = self.wrap_tools_with_hooks(
            tools_arc,
            descriptor.id.clone(),
            request.session_id.clone(),
            request.metadata_json.clone(),
        );

        let tools_for_registry = Arc::new(tools_arc.clone());
        let registry: Arc<dyn ToolRegistry> = Arc::new(RuntimeToolRegistry::new(tools_for_registry));

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

        let mut agent_builder = Agent::builder()
            .llm(self.provider.clone())
            .with_tools(all_tools)
            .memory(Box::new(memory))
            .system_prompt(&system_prompt)
            .session_id(request.session_id.clone())
            .tool_context_metadata(trace_context.to_metadata_map());
        if let Some(source) = self.canvas_observation_source_for(&descriptor) {
            agent_builder = agent_builder.observation_source(source);
        }
        let mut agent = agent_builder.build()?;

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
                    AgentEvent::ToolPermissionRequest(permission_request) => {
                        let record = SpecialistAgentEventRecord {
                            event_type: "tool_permission_request".to_string(),
                            tool_name: Some(permission_request.tool_name.clone()),
                            payload_json: serde_json::json!({ "request": permission_request.clone() }),
                        };
                        self.emit_agent_event_hook(&descriptor, &request, &record)
                            .await;
                        events.push(record);
                        yield SpecialistExecutionEvent::Agent(AgentEvent::ToolPermissionRequest(permission_request));
                    }
                    AgentEvent::ToolPermissionResolved { request_id, approved } => {
                        let record = SpecialistAgentEventRecord {
                            event_type: "tool_permission_resolved".to_string(),
                            tool_name: None,
                            payload_json: serde_json::json!({ "request_id": request_id.clone(), "approved": approved }),
                        };
                        self.emit_agent_event_hook(&descriptor, &request, &record)
                            .await;
                        events.push(record);
                        yield SpecialistExecutionEvent::Agent(AgentEvent::ToolPermissionResolved { request_id, approved });
                    }
                    AgentEvent::Observation { source, kind, payload, timestamp } => {
                        let record = SpecialistAgentEventRecord {
                            event_type: "observation".to_string(),
                            tool_name: None,
                            payload_json: serde_json::json!({
                                "source": source.clone(),
                                "kind": kind.clone(),
                                "payload": payload.clone(),
                                "timestamp": timestamp.clone(),
                            }),
                        };
                        self.emit_agent_event_hook(&descriptor, &request, &record)
                            .await;
                        events.push(record);
                        yield SpecialistExecutionEvent::Agent(AgentEvent::Observation { source, kind, payload, timestamp });
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

        if let Err(err) = crate::background_tasks::BackgroundTaskCoordinator::new(self.pool.clone())
            .on_task_settled(crate::background_tasks::TaskSettledEvent {
                task_id: request.task_id.clone(),
            })
            .await
        {
            tracing::warn!(
                "failed to process async task completion for {}: {}",
                request.task_id,
                err
            );
        }

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
            "workspace_root": workspace_root_from_metadata(metadata_json),
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
                Arc::new(HookedTool::new(
                    tool,
                    self.hook_registry.clone(),
                    self.trace_sink.clone(),
                    specialist_id.clone(),
                    session_id.clone(),
                    metadata_json.clone(),
                )) as Arc<dyn Tool>
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
            "workspace_root": workspace_root_from_metadata(&request.metadata_json),
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
            "workspace_root": workspace_root_from_metadata(&request.metadata_json),
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

fn request_working_directory(request: &SpecialistExecutionRequest) -> Result<PathBuf> {
    if let Some(workdir) = request.workdir.as_deref().filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(workdir));
    }
    working_directory_from_metadata(&request.metadata_json)
}

fn working_directory_from_metadata(metadata_json: &Value) -> Result<PathBuf> {
    metadata_json
        .get("workdir")
        .or_else(|| metadata_json.get("workspace_root"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
        .map_err(Into::into)
}

fn metadata_with_workspace_root(metadata_json: Value, working_directory: &Path) -> Value {
    let workspace_root = working_directory.to_string_lossy().to_string();
    match metadata_json {
        Value::Object(mut map) => {
            map.entry("workdir".to_string())
                .or_insert_with(|| Value::String(workspace_root.clone()));
            map.insert("workspace_root".to_string(), Value::String(workspace_root));
            Value::Object(map)
        }
        other => serde_json::json!({
            "workdir": workspace_root,
            "workspace_root": workspace_root,
            "metadata": other,
        }),
    }
}

fn metadata_object_for_tool_context(
    metadata_json: &Value,
) -> serde_json::Map<String, serde_json::Value> {
    metadata_json.as_object().cloned().unwrap_or_default()
}

fn append_tools_overriding_existing(
    tools: &mut Vec<Arc<dyn Tool>>,
    additional_tools: Vec<Arc<dyn Tool>>,
) {
    for tool in additional_tools {
        let name = tool.name().to_string();
        tools.retain(|existing| existing.name() != name);
        tools.push(tool);
    }
}

fn workspace_root_from_metadata(metadata_json: &Value) -> Option<String> {
    metadata_json
        .get("workspace_root")
        .or_else(|| metadata_json.get("workdir"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        })
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

pub struct GeWuRuntimeBuilder {
    factories: Vec<Box<dyn ToolFactory>>,
    extra_providers: Vec<(String, String, Box<dyn ToolFactory>)>,
    specialists: Vec<SpecialistDescriptor>,
    manifest_dirs: Vec<PathBuf>,
    runtime_plugin_dirs: Vec<PathBuf>,
    skill_dirs: Vec<PathBuf>,
    workflow_dirs: Vec<PathBuf>,
    resource_dirs: Vec<PathBuf>,
    command_tool_dirs: Vec<PathBuf>,
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

    pub fn with_resource_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.resource_dirs.push(dir.into());
        self
    }

    pub fn with_resource_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.resource_dirs.extend(dirs);
        self
    }

    pub fn with_command_tool_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.command_tool_dirs.push(dir.into());
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

        if let Some(factory) = load_resource_tool_factory(&self.resource_dirs) {
            registry.register(Arc::new(ToolFactoryProvider::new(
                "builtin",
                "resources",
                Box::new(factory),
            )));
        }

        // Load command-backed tools from ~/.gewu/tools/ plus any explicitly
        // configured project-local tool directories.
        let mut command_tool_dirs = vec![CommandToolRegistry::default_dir()];
        command_tool_dirs.extend(self.command_tool_dirs);
        for tools_dir in command_tool_dirs {
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

        add_registered_local_capabilities_to_default_specialists(&mut specialists, &registry);

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

fn load_resource_tool_factory(resource_dirs: &[PathBuf]) -> Option<ResourceToolFactory> {
    let mut registry = ResourceRegistry::new();
    let registration_enabled = !resource_dirs.is_empty();
    let mutable_provider =
        registration_enabled.then(|| Arc::new(MutableResourceProvider::new("agent-registered")));
    for dir in default_resource_dirs()
        .into_iter()
        .chain(resource_dirs.iter().cloned())
    {
        if !dir.is_dir() {
            continue;
        }
        match CatalogLoader::load_dir(&dir) {
            Ok(resources) if !resources.is_empty() => {
                tracing::info!(
                    "Loaded {} resource(s) from {}",
                    resources.len(),
                    dir.display()
                );
                if let Some(provider) = &mutable_provider {
                    if let Err(err) = provider.upsert_many(resources) {
                        tracing::warn!(
                            "Failed to load resource catalogs into mutable provider from {}: {}",
                            dir.display(),
                            err
                        );
                    }
                } else {
                    let provider = MutableResourceProvider::new(dir.display().to_string());
                    if let Err(err) = provider.upsert_many(resources) {
                        tracing::warn!(
                            "Failed to load resource catalogs from {}: {}",
                            dir.display(),
                            err
                        );
                    } else {
                        registry.register(provider);
                    }
                }
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(
                "Failed to load resource catalogs from {}: {}",
                dir.display(),
                err
            ),
        }
    }

    match mutable_provider {
        Some(provider) => {
            registry.register(provider.clone());
            Some(ResourceToolFactory::with_registration(
                Arc::new(registry),
                provider,
            ))
        }
        None if registry.provider_count() > 0 => Some(ResourceToolFactory::new(Arc::new(registry))),
        None => None,
    }
}

fn add_registered_local_capabilities_to_default_specialists(
    specialists: &mut [SpecialistDescriptor],
    registry: &CapabilityRegistry,
) {
    let local_capabilities = [
        "resource.describe",
        "resource.resolve",
        "resource.search",
        "resource.validate",
        "resource.register",
        "gene.resolve_symbol",
    ];

    for specialist in specialists {
        if specialist.id != "build" && specialist.id != "general" {
            continue;
        }

        for capability in local_capabilities {
            if registry.contains(capability)
                && !specialist
                    .capabilities
                    .iter()
                    .any(|name| name == capability)
            {
                specialist.capabilities.push(capability.to_string());
            }
        }
    }
}

fn default_resource_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from(".gewu/resources")];
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".gewu/resources"));
    }
    dirs
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

#[cfg(test)]
mod task_result_tests {
    use super::*;
    use crate::test_support::cwd_lock;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use gewu_core::traits::{LlmProvider, Tool};

    use gewu_core::types::{Message, ProviderEvent, SpecialistDescriptor, ToolContext, ToolResult};
    use sqlx::sqlite::SqlitePoolOptions;
    use std::error::Error;
    use std::sync::Arc;

    struct StubLlm;

    #[async_trait]
    impl LlmProvider for StubLlm {
        async fn stream_response<'a>(
            &'a self,
            _messages: &[Message],
            _tools: &[Arc<dyn Tool>],
        ) -> Result<
            BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
            Box<dyn Error + Send + Sync>,
        > {
            unimplemented!("stub provider is not called by skill prompt tests")
        }

        async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
            Err(Box::new(std::io::Error::other(
                "stub provider list_models() should not be called in skill prompt tests",
            )))
        }
    }

    struct TestSpecialistPlugin {
        descriptor: SpecialistDescriptor,
    }

    impl Plugin for TestSpecialistPlugin {
        fn name(&self) -> &str {
            "test-specialist-plugin"
        }

        fn tool_factories(&self) -> Vec<Box<dyn ToolFactory>> {
            Vec::new()
        }

        fn specialist_descriptors(&self) -> Vec<SpecialistDescriptor> {
            vec![self.descriptor.clone()]
        }
    }

    struct CwdRestore {
        original: std::path::PathBuf,
    }

    impl Drop for CwdRestore {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    struct ReplacementWorkflowEmitTool;

    #[async_trait]
    impl Tool for ReplacementWorkflowEmitTool {
        fn name(&self) -> &str {
            "workflow.emit"
        }

        fn description(&self) -> &str {
            "replacement workflow emit"
        }

        fn schema(&self) -> schemars::schema::RootSchema {
            schemars::schema::RootSchema::default()
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> std::result::Result<ToolResult, Box<dyn Error + Send + Sync>> {
            Ok(ToolResult::new(
                "workflow.emit",
                serde_json::json!({"ok": true, "source": "replacement"}).to_string(),
            ))
        }
    }

    fn write_skill(root: &std::path::Path) {
        let skill_dir = root.join(".gewu/skills/gene-locus-resolver");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: gene-locus-resolver
description: Resolve gene symbols before opening Genome Canvas.
capabilities:
  - gene.resolve_symbol
---

Do not guess coordinates.
"#,
        )
        .unwrap();
    }

    fn write_global_skill(root: &std::path::Path) {
        let skill_dir = root.join(".gewu/skills/global-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: global-skill
description: Global skill visible to every specialist.
capabilities:
  - bash_safe
---

Global sentinel body.
"#,
        )
        .unwrap();
    }

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

    #[tokio::test]
    async fn specialist_without_skills_does_not_see_global_skill_summaries() {
        let original_cwd = std::env::current_dir().unwrap();
        let project_root = std::env::temp_dir().join(format!(
            "gewu-runtime-global-skill-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&project_root).unwrap();
        write_global_skill(&project_root);

        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        gewu_storage::run_migrations(&pool).await.unwrap();

        let runtime = {
            let _guard = cwd_lock().lock().unwrap();
            let _restore = CwdRestore {
                original: original_cwd.clone(),
            };
            std::env::set_current_dir(&project_root).unwrap();
            let runtime = GeWuRuntime::builder()
                .provider(Arc::new(StubLlm))
                .pool(pool)
                .with_skill_dir(project_root.join(".gewu/skills"))
                .build()
                .unwrap();
            let _ = std::env::set_current_dir(&original_cwd);
            runtime
        };

        let descriptor = SpecialistDescriptor {
            id: "general".to_string(),
            description: "General specialist".to_string(),
            capabilities: vec!["canvas.open".to_string()],
            skills: vec![],
            system_prompt: "Base prompt.".to_string(),
        };

        let effective = runtime.descriptor_with_skills(&descriptor);
        assert!(
            !effective
                .capabilities
                .iter()
                .any(|capability| capability == "bash_safe")
        );

        let prompt = runtime.append_skill_prompt(&descriptor, descriptor.system_prompt.clone());
        assert!(!prompt.contains("<available_skills>"));
        assert!(!prompt.contains("global-skill"));
        assert!(!prompt.contains("Use the `load_skill` tool"));
        assert!(!prompt.contains("<skill_content name=\"global-skill\""));
        assert!(!prompt.contains("Global sentinel body."));
    }

    #[tokio::test]
    async fn declared_skill_is_recommended_but_not_preloaded() {
        let original_cwd = std::env::current_dir().unwrap();
        let project_root = std::env::temp_dir().join(format!(
            "gewu-runtime-declared-skill-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&project_root).unwrap();
        write_skill(&project_root);
        write_global_skill(&project_root);

        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        gewu_storage::run_migrations(&pool).await.unwrap();

        let runtime = {
            let _guard = cwd_lock().lock().unwrap();
            let _restore = CwdRestore {
                original: original_cwd.clone(),
            };
            std::env::set_current_dir(&project_root).unwrap();
            let runtime = GeWuRuntime::builder()
                .provider(Arc::new(StubLlm))
                .pool(pool)
                .with_skill_dir(project_root.join(".gewu/skills"))
                .build()
                .unwrap();
            let _ = std::env::set_current_dir(&original_cwd);
            runtime
        };

        let descriptor = SpecialistDescriptor {
            id: "genome-canvas-agent".to_string(),
            description: "Genome canvas specialist".to_string(),
            capabilities: vec!["canvas.open".to_string()],
            skills: vec!["gene-locus-resolver".to_string()],
            system_prompt: "Base prompt.".to_string(),
        };

        let effective = runtime.descriptor_with_skills(&descriptor);
        assert!(
            effective
                .capabilities
                .iter()
                .any(|capability| capability == "gene.resolve_symbol")
        );
        assert!(
            !effective
                .capabilities
                .iter()
                .any(|capability| capability == "bash_safe")
        );

        let prompt = runtime.append_skill_prompt(&effective, effective.system_prompt.clone());
        assert!(prompt.contains("<available_skills>"));
        assert!(
            prompt.contains(
                "gene-locus-resolver: Resolve gene symbols before opening Genome Canvas."
            )
        );
        assert!(!prompt.contains("global-skill"));
        assert!(!prompt.contains("Recommended for this specialist:"));
        assert!(prompt.contains("Use the `load_skill` tool"));
        assert!(!prompt.contains("<skill_content name=\"gene-locus-resolver\""));
        assert!(!prompt.contains("Do not guess coordinates."));
    }

    #[tokio::test]
    async fn load_skill_tool_is_only_added_for_specialists_with_known_skills() {
        let original_cwd = std::env::current_dir().unwrap();
        let project_root = std::env::temp_dir().join(format!(
            "gewu-runtime-load-skill-tool-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&project_root).unwrap();
        write_skill(&project_root);
        write_global_skill(&project_root);

        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        gewu_storage::run_migrations(&pool).await.unwrap();

        let runtime = {
            let _guard = cwd_lock().lock().unwrap();
            let _restore = CwdRestore {
                original: original_cwd.clone(),
            };
            std::env::set_current_dir(&project_root).unwrap();
            let runtime = GeWuRuntime::builder()
                .provider(Arc::new(StubLlm))
                .pool(pool)
                .register_builtin_capabilities()
                .register_builtin_specialists()
                .register_plugin(Box::new(TestSpecialistPlugin {
                    descriptor: SpecialistDescriptor {
                        id: "skill-agent".to_string(),
                        description: "Skill agent".to_string(),
                        capabilities: vec![],
                        skills: vec!["global-skill".to_string()],
                        system_prompt: "Base prompt.".to_string(),
                    },
                }))
                .with_skill_dir(project_root.join(".gewu/skills"))
                .build()
                .unwrap();
            let _ = std::env::set_current_dir(&original_cwd);
            runtime
        };

        let (_agent, registry) = runtime
            .build_specialist("general", "session-1".to_string(), vec![])
            .await
            .unwrap();

        assert!(
            !registry
                .list_tools()
                .iter()
                .any(|tool| tool == "load_skill")
        );

        let (_agent, registry) = runtime
            .build_specialist("skill-agent", "session-2".to_string(), vec![])
            .await
            .unwrap();

        assert!(
            registry
                .list_tools()
                .iter()
                .any(|tool| tool == "load_skill")
        );
        let ctx = gewu_core::types::ToolContext::new("session-2", "message-1");
        let output = registry
            .execute(
                "load_skill",
                serde_json::json!({"name": "global-skill"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.output.contains("Global sentinel body."));
        let err = registry
            .execute(
                "load_skill",
                serde_json::json!({"name": "gene-locus-resolver"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Skill is not available to this specialist: gene-locus-resolver")
        );
    }

    fn write_resource_registration_validator_skill(root: &std::path::Path) {
        let skill_dir = root.join(".gewu/skills/resource-registration-validator");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: resource-registration-validator
description: Validate genomic resource registration URIs and enforce UCF/ATAC conventions before registering or opening Genome Canvas.
---

# Resource Registration Validator

## Canonical UCF Mapping

| Assay | Extension | Source Type |
|-------|-----------|-------------|
| atac  | .ucf      | coverage    |
| chip  | .bw       | score       |
| rna   | .bam      | alignment   |

## Self-Correction Rules

1. **Reject filename-as-sample-id**: If `sample_id` matches the filename basename, derive the sample ID from the parent directory.
2. **Enforce `assay=atac` for UCF files**: Reject tracks without explicit `assay` label; suggest `atac` for `.ucf` extensions.
3. **Require `source=coverage` for UCF**: Validate that ATAC tracks specify `source: coverage` in their track definition.
4. **Report missing registry entries**: Before opening Genome Canvas, verify the sample and track URIs exist in the registry via `resource.search`.
"#,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn declared_resource_registration_skill_is_recommended_not_preloaded() {
        let original_cwd = std::env::current_dir().unwrap();
        let project_root = std::env::temp_dir().join(format!(
            "gewu-runtime-resource-reg-validator-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&project_root).unwrap();
        write_resource_registration_validator_skill(&project_root);

        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        gewu_storage::run_migrations(&pool).await.unwrap();

        let runtime = {
            let _guard = cwd_lock().lock().unwrap();
            let _restore = CwdRestore {
                original: original_cwd.clone(),
            };
            std::env::set_current_dir(&project_root).unwrap();
            let runtime = GeWuRuntime::builder()
                .provider(Arc::new(StubLlm))
                .pool(pool)
                .with_skill_dir(project_root.join(".gewu/skills"))
                .build()
                .unwrap();
            let _ = std::env::set_current_dir(&original_cwd);
            runtime
        };

        let descriptor = SpecialistDescriptor {
            id: "genome-resource-canvas-agent".to_string(),
            description: "Genome resource canvas specialist".to_string(),
            capabilities: vec!["canvas.open".to_string()],
            skills: vec!["resource-registration-validator".to_string()],
            system_prompt: "Base prompt.".to_string(),
        };

        let effective = runtime.descriptor_with_skills(&descriptor);

        let prompt = runtime.append_skill_prompt(&effective, effective.system_prompt.clone());
        assert!(
            prompt.contains("<available_skills>"),
            "Skill should be listed in available_skills block"
        );
        assert!(
            prompt.contains(
                "resource-registration-validator: Validate genomic resource registration URIs"
            ),
            "Skill should be recommended in available_skills"
        );
        assert!(
            prompt.contains("Use the `load_skill` tool"),
            "Load skill instruction should be present"
        );
        assert!(
            !prompt.contains("<skill_content name=\"resource-registration-validator\""),
            "Skill body should NOT be preloaded"
        );
        assert!(
            !prompt.contains("Reject filename-as-sample-id"),
            "Skill body content should NOT appear in prompt"
        );
    }

    #[tokio::test]
    async fn additional_tool_overrides_builtin_capability_with_same_name() {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        gewu_storage::run_migrations(&pool).await.unwrap();
        let runtime = GeWuRuntime::builder()
            .provider(Arc::new(StubLlm))
            .pool(pool)
            .register_builtin_capabilities()
            .register_builtin_specialists()
            .build()
            .unwrap();

        let (_agent, registry) = runtime
            .build_specialist(
                "canvas-analyst",
                "session-override".to_string(),
                vec![Arc::new(ReplacementWorkflowEmitTool)],
            )
            .await
            .unwrap();

        let tools = registry.list_tools();
        assert_eq!(
            tools
                .iter()
                .filter(|tool| tool.as_str() == "workflow.emit")
                .count(),
            1
        );
        let output = registry
            .execute(
                "workflow.emit",
                serde_json::json!({"kind": "data_ingested", "payload": {"data_id": "x"}}),
                &ToolContext::new("session-override", "message-1"),
            )
            .await
            .unwrap();
        assert!(output.output.contains("\"source\":\"replacement\""));
    }
}
