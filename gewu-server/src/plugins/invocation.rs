use super::{
    Instance,
    context::{AppContext, validate_output},
};
use anyhow::Result;
use gewu_protocol::plugin::{CallContext, Event, Object, ToolOutput};
use std::sync::Arc;
use tokio::sync::mpsc;

pub(super) async fn invoke_adapter(
    app_context: &AppContext,
    instance: &Arc<Instance>,
    tool: &str,
    args: Object,
    context: CallContext,
    progress: Option<mpsc::Sender<Event>>,
    agent: bool,
    private: bool,
) -> Result<ToolOutput> {
    let output = instance
        .invoke(tool, args, context, progress, agent, private)
        .await?;
    validate_output(app_context, &output)?;
    Ok(output)
}
