use super::Instance;
use anyhow::{Result, bail};
use gewu_protocol::plugin::{CallContext, ResourceRef};
use serde::Serialize;

pub(super) const MAX_APP_CONTEXT_BYTES: usize = 8 * 1024;

/// The bounded, host-owned context shared by Agent calls and UI observability.
/// Package paths, runtime details and service headers are intentionally absent.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AppContext {
    pub workspace_id: String,
    pub session_id: String,
    pub plugin_id: String,
    pub instance_id: String,
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding_resource: Option<ResourceRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<String>,
}

impl AppContext {
    pub(super) fn for_instance(instance: &Instance, session_id: &str) -> Self {
        Self {
            workspace_id: instance.activation.workspace_id.clone(),
            session_id: session_id.into(),
            plugin_id: instance.package.manifest.id.clone(),
            instance_id: instance.activation.instance_id.clone(),
            generation: instance.activation.generation,
            binding_resource: instance.activation.binding_resource.clone(),
            view_id: None,
            view_epoch: None,
            resource: None,
            selection: None,
        }
    }

    pub(super) fn for_view(
        instance: &Instance,
        session_id: &str,
        view_id: &str,
        resource: Option<ResourceRef>,
        selection: Option<String>,
    ) -> Self {
        let mut context = Self::for_instance(instance, session_id);
        context.view_id = Some(view_id.into());
        context.view_epoch = Some(instance.activation.generation);
        context.resource = resource;
        context.selection = selection;
        context
    }

    pub(super) fn call_context(&self, call_id: String, run_id: Option<String>) -> CallContext {
        CallContext {
            instance_id: self.instance_id.clone(),
            generation: self.generation,
            workspace_id: self.workspace_id.clone(),
            call_id,
            session_id: Some(self.session_id.clone()),
            run_id,
            binding_resource: self.binding_resource.clone(),
            resource: self.resource.clone(),
            view_id: self.view_id.clone(),
            view_epoch: self.view_epoch,
        }
    }
}

pub(super) fn validate_output(
    context: &AppContext,
    output: &gewu_protocol::plugin::ToolOutput,
) -> Result<()> {
    let Some(resource) = context.resource.as_ref() else {
        return Ok(());
    };
    let Some(expected) = resource.revision.as_deref() else {
        return Ok(());
    };
    let gewu_protocol::plugin::ToolOutput::Success {
        changes: Some(changes),
        ..
    } = output
    else {
        return Ok(());
    };
    for change in changes {
        if change.uri == resource.uri
            && change
                .before_revision
                .as_deref()
                .is_some_and(|before| before != expected)
        {
            bail!(
                "STALE_RESOURCE: expected revision {expected}, plugin reported {}",
                change.before_revision.as_deref().unwrap_or_default()
            );
        }
    }
    Ok(())
}
