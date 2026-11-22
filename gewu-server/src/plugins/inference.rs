use super::{Instance, PluginHost, context::AppContext, package};
use anyhow::{Context, Result, ensure};
use gewu_core::{
    traits::{InferenceContext, InferenceContextSource},
    types::ToolContext,
};
use gewu_protocol::plugin::{Effect, ResourceRef, ToolOutput};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::Duration,
};

const SNAPSHOT_KEY: &str = "gewu.plugin.inference";
static SNAPSHOT_SCHEMA: LazyLock<jsonschema::Validator> = LazyLock::new(|| {
    let schema = serde_json::from_str(include_str!(
        "../../../gewu-protocol/schema/application-context.v1.schema.json"
    ))
    .expect("valid embedded JSON");
    package::validator(&schema).expect("valid embedded schema")
});

struct PluginContextSource {
    host: Arc<PluginHost>,
    workspace: String,
    root: PathBuf,
    session: String,
}

impl PluginHost {
    pub fn context_source(
        self: &Arc<Self>,
        workspace: &str,
        root: PathBuf,
        session: &str,
    ) -> Arc<dyn InferenceContextSource> {
        Arc::new(PluginContextSource {
            host: self.clone(),
            workspace: workspace.into(),
            root,
            session: session.into(),
        })
    }
}

#[async_trait::async_trait]
impl InferenceContextSource for PluginContextSource {
    async fn inference_context(
        &self,
    ) -> Result<InferenceContext, Box<dyn std::error::Error + Send + Sync>> {
        use super::context::MAX_APP_CONTEXT_BYTES;

        let active = self
            .host
            .active_for_session(&self.workspace, &self.root, Some(&self.session))
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let pending = active.iter()
            .filter(|instance| instance.package.manifest.scope != gewu_protocol::plugin::Scope::Resource
                && instance.package.manifest.context_provider.is_some())
            .map(|instance| async move {
                let data = tokio::time::timeout_at(deadline, snapshot(instance, &self.session))
                    .await.ok().and_then(Result::ok);
                (instance, data)
            });
        let mut displayed = Vec::new();
        let mut pins = serde_json::Map::new();
        let mut bytes = 64;
        let mut omitted = 0;
        for (instance, data) in futures::future::join_all(pending).await {
            let id = &instance.package.manifest.id;
            let (mut entry, mut pin) = if let Some(data) = data {
                let pin = (instance.activation.instance_id.clone(), data["contextRef"].clone());
                (json!({"pluginId":id,"snapshot":data}), Some(pin))
            } else {
                (json!({"pluginId":id,"status":"unavailable"}), None)
            };
            let mut size = serde_json::to_vec(&entry)?.len() + 1;
            if bytes + size > MAX_APP_CONTEXT_BYTES {
                entry = json!({"pluginId":id,"status":"omitted (context budget)"});
                size = serde_json::to_vec(&entry)?.len() + 1;
                pin = None;
            }
            if bytes + size > MAX_APP_CONTEXT_BYTES {
                omitted += 1;
                continue;
            }
            bytes += size;
            displayed.push(entry);
            if let Some((key, value)) = pin {
                pins.insert(key, value);
            }
        }
        if displayed.is_empty() && omitted == 0 {
            return Ok(InferenceContext::default());
        }
        Ok(InferenceContext {
            text: serde_json::to_string(
                &json!({"providers":displayed,"omittedProviders":omitted}),
            )?,
            metadata: [(
                SNAPSHOT_KEY.into(),
                json!({
                    "inferenceId": uuid::Uuid::new_v4().to_string(), "workspaceId":self.workspace,
                    "sessionId":self.session, "pins":pins,
                }),
            )]
            .into(),
        })
    }
}

async fn snapshot(instance: &Arc<Instance>, session: &str) -> Result<Value> {
    let provider = instance
        .package
        .manifest
        .context_provider
        .as_ref()
        .context("not a context provider")?;
    let context = AppContext::for_instance(instance, session);
    let output = instance
        .invoke(
            &provider.tool,
            Default::default(),
            context.call_context(uuid::Uuid::new_v4().to_string(), None),
            None,
            true,
            false,
        )
        .await?;
    let ToolOutput::Success { data, .. } = output else {
        anyhow::bail!("provider unavailable")
    };
    let value = Value::Object(data);
    package::validate(&SNAPSHOT_SCHEMA, &value)?;
    ensure!(
        value["contextRef"]["revision"].is_string(),
        "contextRef revision must be present"
    );
    Ok(value)
}

impl PluginHost {
    pub(super) async fn pin_agent_call(
        &self,
        app: &mut AppContext,
        instance: &Instance,
        tool: &str,
        ctx: &ToolContext,
    ) -> Result<()> {
        let definition = instance
            .definitions()
            .find(|d| d.name == tool)
            .context("unknown plugin tool")?;
        ensure!(
            definition.expose_to_agent,
            "tool is not exposed to the agent"
        );
        if instance.package.manifest.context_provider.is_none() {
            return Ok(());
        }
        let profile = self.session_state(&app.session_id, &app.plugin_id).await?;
        ensure!(
            super::tools::group_enabled(&profile, &definition.group),
            "PLUGIN_PROFILE_DISABLED: this session has not enabled the {} tool group",
            definition.group
        );
        let meta = ctx.metadata.get(SNAPSHOT_KEY).filter(|value| {
            value["sessionId"] == app.session_id && value["workspaceId"] == app.workspace_id
        });
        if let Some(meta) = meta {
            app.inference_id = meta["inferenceId"].as_str().map(str::to_owned);
            if let Some(value) = meta["pins"].get(&app.instance_id) {
                app.context_ref = Some(serde_json::from_value::<ResourceRef>(value.clone())?);
            }
        }
        ensure!(
            definition.effect == Effect::Read || app.context_ref.is_some(),
            "CONTEXT_UNAVAILABLE: obtain a current application snapshot before making changes"
        );
        Ok(())
    }
}
