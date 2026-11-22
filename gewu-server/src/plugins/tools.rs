use super::{Instance, PluginHost, actions::clip, invocation};
use gewu_core::{
    traits::Tool,
    types::{ToolContext, ToolResult},
};
use gewu_protocol::plugin::*;
use schemars::schema::RootSchema;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, error::Error, path::PathBuf, sync::Arc};
use tokio::sync::mpsc;

struct PluginTool {
    name: String,
    description: String,
    schema: RootSchema,
    action: Action,
}

struct ApplicationSkills(gewu_tools::skill::SkillTool);

#[async_trait::async_trait]
impl Tool for ApplicationSkills {
    fn name(&self) -> &str {
        "plugin_skill"
    }
    fn description(&self) -> &str {
        self.0.description()
    }
    fn schema(&self) -> RootSchema {
        self.0.schema()
    }
    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        self.0.execute(args, ctx).await
    }
}

enum Action {
    Typed(Arc<PluginHost>, Arc<Instance>, String),
    Gateway(Arc<PluginHost>, String, PathBuf, String, Gateway),
}

pub(super) fn group_enabled(state: &super::SessionPluginState, group: &str) -> bool {
    let has = |profile: &str| state.profiles.iter().any(|item| item == profile);
    match group {
        "workspace" => has("workspace-basic"),
        "analysis" => has("analysis-read"),
        "workflow" => has("workflow-authoring"),
        "references" => has("references"),
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum Gateway {
    List,
    Activate,
    Open,
    Close,
    Describe,
    Call,
    SetEnabled,
    SetSession,
    Calls,
    CallStatus,
    Cancel,
    Focus,
}

/// Ordinary tools supplied to the existing Agent at the start of a turn.
impl PluginHost {
    pub(super) async fn agent_tool<'a>(
        &self,
        instance: &'a Instance,
        session: &str,
        name: &str,
    ) -> anyhow::Result<&'a ToolDefinition> {
        let tool = instance
            .definitions()
            .find(|tool| tool.name == name)
            .ok_or_else(|| anyhow::anyhow!("unknown plugin tool"))?;
        anyhow::ensure!(tool.expose_to_agent, "tool is not exposed to the agent");
        let state = self
            .session_state(session, &instance.package.manifest.id)
            .await?;
        anyhow::ensure!(
            group_enabled(&state, &tool.group),
            "PLUGIN_PROFILE_DISABLED: this session has not enabled the {} tool group",
            tool.group
        );
        Ok(tool)
    }

    pub async fn tools(
        self: &Arc<Self>,
        workspace: &str,
        root: PathBuf,
        session: &str,
        existing: &[Arc<dyn Tool>],
    ) -> Vec<Arc<dyn Tool>> {
        if !self.has_configured() {
            return vec![];
        }
        let mut names: HashSet<_> = existing.iter().map(|t| t.name().to_owned()).collect();
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let protocol: Value = serde_json::from_str(SCHEMA).expect("embedded plugin schema");
        let mut resource_schema = protocol["$defs"]["ResourceRef"].clone();
        // Inline ResourceRef's scalar definitions so provider tool schemas need no external refs.
        for property in resource_schema["properties"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            let reference = property["$ref"].as_str().unwrap();
            *property = protocol.pointer(&reference[1..]).unwrap().clone();
        }
        resource_schema["properties"]["uri"]["description"] = json!(
            "Existing path relative to workspaceRoot, using workspace:/// and percent-encoding spaces; e.g. workspace:///demo-scene.svg. Do not use file:// or an absolute filesystem path."
        );
        resource_schema["properties"]["type"]["description"] = json!(
            "Use the matching resourceTypes entry from plugin_list views, e.g. inkscape.document."
        );
        for (kind, name, description, properties, required) in [
            (
                Gateway::List,
                "plugin_list",
                "List configured applications and the instances open in this workspace.",
                json!({}),
                json!([]),
            ),
            (
                Gateway::Activate,
                "plugin_activate",
                "Use a configured application for this session without opening Preview. Then use plugin_describe and plugin_call with the returned instance id.",
                json!({"pluginId":{"type":"string"}}),
                json!(["pluginId"]),
            ),
            (
                Gateway::Open,
                "plugin_open",
                "Open a configured application and show it in Preview. Call plugin_list to discover pluginId, scope and resourceTypes. Resource-scoped plugins require bindingResource, e.g. {\"uri\":\"workspace:///demo-scene.svg\",\"type\":\"inkscape.document\"}; create the file/directory first. Omit bindingResource for session/workspace-scoped plugins such as gewu-browser. Use the returned id with plugin_describe and plugin_call.",
                json!({"pluginId":{"type":"string"},"bindingResource":resource_schema}),
                json!(["pluginId"]),
            ),
            (
                Gateway::Close,
                "plugin_close",
                "Close an open application instance and its Preview views. Files the application wrote stay on disk.",
                json!({"instanceId":{"type":"string"}}),
                json!(["instanceId"]),
            ),
            (
                Gateway::Describe,
                "plugin_describe",
                "Read input/output schemas for a tool listed by an already-open application. instanceId must be an actual id from plugin_open or plugin_list. This does not describe host tools such as plugin_open.",
                json!({"instanceId":{"type":"string"},"tool":{"type":"string"}}),
                json!(["instanceId", "tool"]),
            ),
            (
                Gateway::Call,
                "plugin_call",
                "Call an exposed application tool after reading its schema with plugin_describe. Pass async:true to return a callId immediately and poll plugin_call_status.",
                json!({"instanceId":{"type":"string"},"tool":{"type":"string"},"args":{"type":"object"},"async":{"type":"boolean"}}),
                json!(["instanceId", "tool", "args"]),
            ),
            (
                Gateway::SetEnabled,
                "plugin_set_enabled",
                "Enable or disable an application for this workspace. Disabling closes its running instances and is remembered across restarts.",
                json!({"pluginId":{"type":"string"},"enabled":{"type":"boolean"}}),
                json!(["pluginId", "enabled"]),
            ),
            (
                Gateway::SetSession,
                "plugin_set_session",
                "Override an application's enable state for this session only. Pass enabled:null to follow the workspace-wide setting again, and profiles to choose which tool groups are exposed.",
                json!({"pluginId":{"type":"string"},"enabled":{"type":["boolean","null"]},"profiles":{"type":"array","items":{"type":"string"}}}),
                json!(["pluginId"]),
            ),
            (
                Gateway::Calls,
                "plugin_calls",
                "List this session's application calls, including finished ones kept for inspection.",
                json!({}),
                json!([]),
            ),
            (
                Gateway::CallStatus,
                "plugin_call_status",
                "Read one application call started by this session. Returns result once it settles, otherwise progress and status.",
                json!({"callId":{"type":"string"}}),
                json!(["callId"]),
            ),
            (
                Gateway::Cancel,
                "plugin_cancel",
                "Request cancellation of a running application call started by this session.",
                json!({"callId":{"type":"string"}}),
                json!(["callId"]),
            ),
            (
                Gateway::Focus,
                "plugin_focus",
                "Make an open application instance the focused view for this session, so later calls and context follow it.",
                json!({"instanceId":{"type":"string"}}),
                json!(["instanceId"]),
            ),
        ] {
            if names.insert(name.into()) {
                let schema = serde_json::from_value(json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})).unwrap();
                tools.push(Arc::new(PluginTool {
                    name: name.into(),
                    description: description.into(),
                    schema,
                    action: Action::Gateway(
                        self.clone(),
                        workspace.into(),
                        root.clone(),
                        session.into(),
                        kind,
                    ),
                }));
            }
        }
        let active = self
            .active_for_session(workspace, &root, Some(session))
            .await;
        let mut seen = HashSet::new();
        // Package instructions are metadata; reading them must not start a backend.
        let packages: Vec<_> = self
            .entries
            .read()
            .unwrap()
            .values()
            .filter_map(|(entry, package)| {
                package
                    .clone()
                    .or_else(|| super::package::Package::read(&entry.package).ok())
            })
            .collect();
        let skills: Vec<_> = packages
            .iter()
            .flat_map(|package| &package.skills)
            .filter(|s| seen.insert(s.name.clone()))
            .cloned()
            .collect();
        if !skills.is_empty() && names.insert("plugin_skill".into()) {
            tools.push(Arc::new(ApplicationSkills(
                gewu_tools::skill::SkillTool::new(skills),
            )));
        }
        for instance in active {
            let state = self
                .session_state(session, &instance.package.manifest.id)
                .await
                .unwrap_or_default();
            // Resource-scoped tools are selected explicitly through the gateway.
            if instance.package.manifest.scope != Scope::Workspace {
                if instance.package.manifest.scope != Scope::Session {
                    continue;
                }
            }
            for definition in instance.definitions().filter(|t| t.expose_to_agent) {
                if !group_enabled(&state, &definition.group) {
                    continue;
                }
                let plugin = &instance.package.manifest.id;
                let native = &definition.name;
                let digest = format!("{:x}", Sha256::digest(format!("{plugin}\0{native}")));
                let human: String = format!("{plugin}_{native}")
                    .chars()
                    .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
                    .take(46)
                    .collect();
                let preferred = if plugin == "tachyon" {
                    format!("tachyon.{native}")
                } else {
                    format!("plg_{human}_{}", &digest[..12])
                };
                let name = if names.contains(&preferred) && plugin == "tachyon" {
                    format!("plg_{human}_{}", &digest[..12])
                } else {
                    preferred
                };
                if !names.insert(name.clone()) {
                    continue;
                }
                if let Ok(schema) =
                    serde_json::from_value(Value::Object(definition.input_schema.clone()))
                {
                    tools.push(Arc::new(PluginTool {
                        name,
                        description: definition.description.clone(),
                        schema,
                        action: Action::Typed(self.clone(), instance.clone(), native.clone()),
                    }));
                }
            }
        }
        tools
    }

    async fn agent_instance(
        &self,
        mut value: Value,
        workspace: &str,
        root: &std::path::Path,
        session: &str,
    ) -> anyhow::Result<Value> {
        let active = self
            .active_for_session(workspace, root, Some(session))
            .await;
        let instance = active
            .iter()
            .find(|instance| value["id"] == instance.activation.instance_id)
            .ok_or_else(|| anyhow::anyhow!("application instance is no longer active"))?;
        let state = self
            .session_state(session, &instance.package.manifest.id)
            .await?;
        value["tools"] = json!(
            instance
                .definitions()
                .filter(|tool| tool.expose_to_agent && group_enabled(&state, &tool.group))
                .map(|tool| &tool.name)
                .collect::<Vec<_>>()
        );
        value["scope"] = json!(instance.package.manifest.scope);
        if instance.package.manifest.scope == Scope::Session {
            value["openHint"] = json!(
                "Session-scoped: use plugin_open with pluginId only, without bindingResource. Opening Preview does not create application content; follow the package instructions."
            );
        }
        value["instructions"] = json!(instance.package.skills.iter().take(1).map(|skill| {
            let end = skill.content.floor_char_boundary(6000);
            json!({"name":skill.name,"content":&skill.content[..end],"complete":end == skill.content.len(),
                "readMore":{"tool":"plugin_skill","args":{"name":skill.name}}})
        }).collect::<Vec<_>>());
        Ok(value)
    }
}

#[async_trait::async_trait]
impl Tool for PluginTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn schema(&self) -> RootSchema {
        self.schema.clone()
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        if ctx.is_aborted() {
            return Err(anyhow::anyhow!("tool call was cancelled").into());
        }
        match &self.action {
            Action::Typed(host, instance, tool) => {
                Ok(invoke(host, instance, tool, args, ctx).await?)
            }
            Action::Gateway(host, workspace, root, session, kind) => {
                let text = |key| {
                    args.get(key)
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| anyhow::anyhow!("missing {key}"))
                };
                let live = || host.active_for_session(workspace, root, Some(session));
                let data = match kind {
                    Gateway::List => {
                        let mut catalog = host
                            .action(workspace, root, session, HostAction::List)
                            .await?;
                        if let Some(instances) = catalog["instances"].as_array_mut() {
                            for instance in instances {
                                let plugin = instance["pluginId"].as_str().ok_or_else(|| {
                                    anyhow::anyhow!("instance is missing pluginId")
                                })?;
                                let state = host.session_state(session, plugin).await?;
                                if let Some(tools) = instance["tools"].as_array_mut() {
                                    *tools = tools
                                        .iter()
                                        .filter(|tool| tool["exposeToAgent"] == true)
                                        .filter(|tool| {
                                            group_enabled(
                                                &state,
                                                tool["group"].as_str().unwrap_or("workspace"),
                                            )
                                        })
                                        .filter_map(|tool| {
                                            tool["name"].as_str().map(|name| json!(name))
                                        })
                                        .collect();
                                }
                            }
                        }
                        catalog
                    }
                    Gateway::Activate => {
                        let instance = host
                            .action(
                                workspace,
                                root,
                                session,
                                HostAction::Activate {
                                    plugin_id: text("pluginId")?.into(),
                                },
                            )
                            .await?;
                        host.agent_instance(instance, workspace, root, session)
                            .await?
                    }
                    Gateway::Open => {
                        let resource = args
                            .get("bindingResource")
                            .cloned()
                            .map(serde_json::from_value)
                            .transpose()?;
                        let mut instance = host
                            .action(
                                workspace,
                                root,
                                session,
                                HostAction::Open {
                                    plugin_id: text("pluginId")?.into(),
                                    binding_resource: resource,
                                },
                            )
                            .await?;
                        instance["workspaceRoot"] = root.to_string_lossy().into_owned().into();
                        host.agent_instance(instance, workspace, root, session)
                            .await?
                    }
                    Gateway::Close => {
                        host.action(
                            workspace,
                            root,
                            session,
                            HostAction::Close {
                                instance_id: text("instanceId")?.into(),
                            },
                        )
                        .await?
                    }
                    Gateway::Describe | Gateway::Call => {
                        let id = text("instanceId")?;
                        let tool = text("tool")?;
                        let instance = live()
                            .await
                            .into_iter()
                            .find(|i| i.activation.instance_id == id)
                            .ok_or_else(|| {
                                anyhow::anyhow!("plugin instance is not ready in this workspace; call plugin_open or plugin_list and use an actual returned instance id, then choose a tool from that instance's tools list")
                            })?;
                        if matches!(kind, Gateway::Call) {
                            if args.get("async").and_then(Value::as_bool) == Some(true) {
                                let call = host
                                    .action_with_agent(
                                        workspace,
                                        root,
                                        session,
                                        HostAction::Call {
                                            instance_id: instance.activation.instance_id.clone(),
                                            tool: tool.into(),
                                            args: args
                                                .get("args")
                                                .and_then(Value::as_object)
                                                .cloned()
                                                .unwrap_or_default(),
                                            view_id: None,
                                            context_ref: None,
                                        },
                                        Some(ctx),
                                    )
                                    .await?;
                                return Ok(ToolResult::new(
                                    &self.name,
                                    serde_json::to_string(&call)?,
                                ));
                            }
                            return Ok(invoke(
                                host,
                                &instance,
                                tool,
                                args.get("args").cloned().unwrap_or(Value::Null),
                                ctx,
                            )
                            .await?);
                        }
                        serde_json::to_value(host.agent_tool(&instance, session, tool).await?)?
                    }
                    Gateway::SetEnabled => {
                        let enabled = args
                            .get("enabled")
                            .and_then(Value::as_bool)
                            .ok_or_else(|| anyhow::anyhow!("missing enabled"))?;
                        host.action(
                            workspace,
                            root,
                            session,
                            HostAction::SetEnabled {
                                plugin_id: text("pluginId")?.into(),
                                enabled,
                            },
                        )
                        .await?
                    }
                    Gateway::SetSession => {
                        let plugin_id = text("pluginId")?.to_owned();
                        let enabled = args.get("enabled").and_then(Value::as_bool);
                        if args
                            .get("enabled")
                            .is_some_and(|value| !value.is_null() && !value.is_boolean())
                        {
                            return Err(anyhow::anyhow!("enabled must be a boolean or null").into());
                        }
                        // Omitting profiles keeps the session's current ones; a
                        // toggle must not silently drop workflow-authoring.
                        let profiles = match args.get("profiles") {
                            Some(value) => serde_json::from_value(value.clone())?,
                            None => {
                                host.session_state_for(session, &plugin_id)
                                    .await
                                    .unwrap_or_default()
                                    .profiles
                            }
                        };
                        host.action(
                            workspace,
                            root,
                            session,
                            HostAction::SetSession {
                                plugin_id,
                                enabled,
                                profiles,
                            },
                        )
                        .await?
                    }
                    Gateway::Calls => {
                        host.action(workspace, root, session, HostAction::Calls)
                            .await?
                    }
                    Gateway::CallStatus => {
                        host.action(
                            workspace,
                            root,
                            session,
                            HostAction::CallStatus {
                                call_id: text("callId")?.into(),
                            },
                        )
                        .await?
                    }
                    Gateway::Cancel => {
                        host.action(
                            workspace,
                            root,
                            session,
                            HostAction::Cancel {
                                call_id: text("callId")?.into(),
                            },
                        )
                        .await?
                    }
                    Gateway::Focus => {
                        host.action(
                            workspace,
                            root,
                            session,
                            HostAction::FocusInstance {
                                instance_id: text("instanceId")?.into(),
                            },
                        )
                        .await?
                    }
                };
                let text = serde_json::to_string(&data)?;
                if text.len() > 16384 {
                    return Err(anyhow::anyhow!(
                        "catalog or schema exceeds the model context budget"
                    )
                    .into());
                }
                Ok(ToolResult::new(&self.name, text))
            }
        }
    }
}

struct CancelOnDrop {
    adapter: Option<super::stdio::Adapter>,
    id: String,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(adapter) = &self.adapter
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let adapter = adapter.clone();
            let call_id = self.id.clone();
            runtime.spawn(async move {
                let _ = adapter.request(Operation::Cancel { call_id }, None).await;
            });
        }
    }
}

async fn invoke(
    host: &PluginHost,
    instance: &Arc<Instance>,
    tool: &str,
    args: Value,
    ctx: &ToolContext,
) -> anyhow::Result<ToolResult> {
    let root = ctx
        .working_directory
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing workspace context"))?
        .canonicalize()?;
    anyhow::ensure!(
        root == PathBuf::from(&instance.activation.workspace_root),
        "cross-workspace plugin tool call"
    );
    let args = args
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("tool args must be an object"))?;
    let id = uuid::Uuid::new_v4().to_string();
    let mut app_context = host.resolve_context(
        &instance.activation.workspace_id,
        &ctx.session_id,
        instance,
        None,
    )?;
    host.pin_agent_call(&mut app_context, instance, tool, ctx)
        .await?;
    let context = app_context.call_context(id.clone(), None);
    let (tx, mut rx) = mpsc::channel(8);
    let mut guard = CancelOnDrop {
        adapter: Some(instance.adapter.clone()),
        id: id.clone(),
    };
    let work = invocation::invoke_adapter(
        &app_context,
        instance,
        tool,
        args,
        context,
        Some(tx),
        true,
        false,
    );
    tokio::pin!(work);
    let output = loop {
        tokio::select! {
            result = &mut work => break result,
            Some(Event::Progress {message,..}) = rx.recv() => ctx.report_progress(clip(&message,2048)),
            _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {
                anyhow::ensure!(!ctx.is_aborted(), "plugin cancellation requested");
            }
        }
    };
    if output.as_ref().is_ok()
        || output
            .as_ref()
            .is_err_and(|e| !e.to_string().starts_with("OUTCOME_UNKNOWN"))
    {
        guard.adapter = None;
    }
    let output = output?;
    let data = serde_json::to_value(output)?;
    let mut metadata = data.clone();
    metadata["context"] = serde_json::to_value(&app_context)?;
    let text = serde_json::to_string(&data)?;
    let mut result = ToolResult::new(
        tool,
        if text.len() <= 8192 {
            text
        } else {
            format!("{}… [summary truncated]", clip(&text, 8130))
        },
    );
    let stored = json!({"workspaceId":instance.activation.workspace_id,"pluginId":instance.package.manifest.id,
        "bindingResource":instance.activation.binding_resource,"context":app_context,"result":data});
    match ctx
        .session_state
        .write(
            &ctx.session_id,
            &format!("gewu.plugin.result.{id}"),
            &stored,
        )
        .await
    {
        Ok(()) => result
            .output
            .push_str(&format!("\n[GeWu plugin result: {id}]")),
        Err(error) => tracing::warn!("could not persist plugin tool output: {error}"),
    }
    result.metadata.insert("gewu.plugin".into(), metadata);
    result.metadata.insert(
        "gewu.plugin.instanceId".into(),
        instance.activation.instance_id.clone().into(),
    );
    Ok(result)
}
