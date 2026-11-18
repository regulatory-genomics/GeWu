use super::{Instance, PluginHost, actions::clip};
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
    Gateway(Arc<PluginHost>, String, PathBuf, Gateway),
}

#[derive(Clone, Copy)]
enum Gateway {
    List,
    Open,
    Describe,
    Call,
}

/// Ordinary tools supplied to the existing Agent at the start of a turn.
impl PluginHost {
    pub async fn tools(
        self: &Arc<Self>,
        workspace: &str,
        root: PathBuf,
        existing: &[Arc<dyn Tool>],
    ) -> Vec<Arc<dyn Tool>> {
        if !self.has_enabled() {
            return vec![];
        }
        let mut names: HashSet<_> = existing.iter().map(|t| t.name().to_owned()).collect();
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for (kind, name, description, properties, required) in [
            (
                Gateway::List,
                "plugin_list",
                "List configured applications and the instances open in this workspace.",
                json!({}),
                json!([]),
            ),
            (
                Gateway::Open,
                "plugin_open",
                "Open a configured application in this workspace; resource-scoped plugins need a bindingResource.",
                json!({"pluginId":{"type":"string"},"bindingResource":{"type":"object"}}),
                json!(["pluginId"]),
            ),
            (
                Gateway::Describe,
                "plugin_describe",
                "Read input/output schemas for an exposed tool in an active application instance.",
                json!({"instanceId":{"type":"string"},"tool":{"type":"string"}}),
                json!(["instanceId", "tool"]),
            ),
            (
                Gateway::Call,
                "plugin_call",
                "Call an exposed application tool after reading its schema with plugin_describe.",
                json!({"instanceId":{"type":"string"},"tool":{"type":"string"},"args":{"type":"object"}}),
                json!(["instanceId", "tool", "args"]),
            ),
        ] {
            if names.insert(name.into()) {
                let schema = serde_json::from_value(json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})).unwrap();
                tools.push(Arc::new(PluginTool {
                    name: name.into(),
                    description: description.into(),
                    schema,
                    action: Action::Gateway(self.clone(), workspace.into(), root.clone(), kind),
                }));
            }
        }
        let active = self.active(workspace, &root).await;
        let mut seen = HashSet::new();
        let skills: Vec<_> = active
            .iter()
            .flat_map(|i| &i.package.skills)
            .filter(|s| seen.insert(s.name.clone()))
            .cloned()
            .collect();
        if !skills.is_empty() && names.insert("plugin_skill".into()) {
            tools.push(Arc::new(ApplicationSkills(
                gewu_tools::skill::SkillTool::new(skills),
            )));
        }
        for instance in active {
            // Resource-scoped tools are selected explicitly through the gateway.
            if instance.package.manifest.scope != Scope::Workspace {
                continue;
            }
            for definition in instance.definitions().filter(|t| t.expose_to_agent) {
                let plugin = &instance.package.manifest.id;
                let native = &definition.name;
                let digest = format!("{:x}", Sha256::digest(format!("{plugin}\0{native}")));
                let human: String = format!("{plugin}_{native}")
                    .chars()
                    .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
                    .take(46)
                    .collect();
                let name = format!("plg_{human}_{}", &digest[..12]);
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
            Action::Gateway(host, workspace, root, kind) => {
                let text = |key| {
                    args.get(key)
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| anyhow::anyhow!("missing {key}"))
                };
                let live = || host.active(workspace, root);
                let data = match kind {
                    Gateway::List => {
                        json!({"plugins":host.catalog(),"instances":live().await.iter().map(|i|json!({"id":i.activation.instance_id,"pluginId":i.package.manifest.id,"bindingResource":i.activation.binding_resource})).collect::<Vec<_>>()})
                    }
                    Gateway::Open => {
                        let resource = args
                            .get("bindingResource")
                            .cloned()
                            .map(serde_json::from_value)
                            .transpose()?;
                        let i = host
                            .open(text("pluginId")?, workspace, root, resource)
                            .await?;
                        json!({"id":i.activation.instance_id,"pluginId":i.package.manifest.id,"tools":i.definitions().filter(|t|t.expose_to_agent).map(|t|&t.name).collect::<Vec<_>>()})
                    }
                    Gateway::Describe | Gateway::Call => {
                        let id = text("instanceId")?;
                        let tool = text("tool")?;
                        let instance = live()
                            .await
                            .into_iter()
                            .find(|i| i.activation.instance_id == id)
                            .ok_or_else(|| {
                                anyhow::anyhow!("plugin instance is not ready in this workspace")
                            })?;
                        if matches!(kind, Gateway::Call) {
                            return Ok(invoke(
                                host,
                                &instance,
                                tool,
                                args.get("args").cloned().unwrap_or(Value::Null),
                                ctx,
                            )
                            .await?);
                        }
                        serde_json::to_value(
                            instance
                                .definitions()
                                .find(|t| t.expose_to_agent && t.name == tool)
                                .ok_or_else(|| anyhow::anyhow!("tool is not exposed"))?,
                        )?
                    }
                };
                let text = serde_json::to_string(&data)?;
                if text.len() > 8192 {
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
    let mut context = instance.context(id.clone(), Some(ctx.session_id.clone()));
    context.run_id = Some(ctx.run_id.clone());
    host.selected_view(instance, &ctx.session_id, &mut context);
    let (tx, mut rx) = mpsc::channel(8);
    let mut guard = CancelOnDrop {
        adapter: Some(instance.adapter.clone()),
        id: id.clone(),
    };
    let work = instance.invoke(tool, args, context, Some(tx), true, false);
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
        "bindingResource":instance.activation.binding_resource,"result":data});
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
    result.metadata.insert("gewu.plugin".into(), data);
    result.metadata.insert(
        "gewu.plugin.instanceId".into(),
        instance.activation.instance_id.clone().into(),
    );
    Ok(result)
}
