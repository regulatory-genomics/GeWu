use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ExecutionRunKind, ToolContext, ToolResult};
use serde_json::Value;
use std::sync::Arc;

use crate::execution_trace::{StartTraceRun, TraceSink};
use crate::hooks::HookRegistry;
use crate::manifest::schema::RuntimeHookEvent;

pub struct HookedTool {
    inner: Arc<dyn Tool>,
    hook_registry: Arc<HookRegistry>,
    trace_sink: Arc<dyn TraceSink>,
    specialist_id: String,
    session_id: String,
    metadata_json: Value,
}

impl HookedTool {
    pub fn new(
        inner: Arc<dyn Tool>,
        hook_registry: Arc<HookRegistry>,
        trace_sink: Arc<dyn TraceSink>,
        specialist_id: String,
        session_id: String,
        metadata_json: Value,
    ) -> Self {
        Self {
            inner,
            hook_registry,
            trace_sink,
            specialist_id,
            session_id,
            metadata_json,
        }
    }
}

#[async_trait]
impl Tool for HookedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn schema(&self) -> schemars::schema::RootSchema {
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
            "workspace_root": workspace_root_from_metadata(&self.metadata_json),
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
