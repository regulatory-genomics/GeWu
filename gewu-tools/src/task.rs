//! Task delegation tool.
//!
//! This module provides the [`TaskTool`] which allows the agent to launch
//! sub-agents to handle complex, multistep tasks. Agent construction is
//! inverted through the [`AgentFactory`] trait: the factory owns provider,
//! memory, and toolset wiring, while the task tool only drives the
//! sub-agent to completion.
//!
//! # Example
//!
//! ```ignore
//! use gewu_tools::task::{AgentFactory, TaskTool};
//!
//! let tool = TaskTool::new(Arc::new(MyFactory), vec![
//!     ("build".to_string(), "Runs commands and edits files".to_string()),
//! ]);
//! ```

use async_trait::async_trait;
use gewu_core::Agent;
use gewu_core::types::{AgentEvent, ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::Deserialize;
use std::error::Error;
use std::sync::Arc;

use gewu_core::traits::Tool;

/// Creates sub-agents for the [`TaskTool`].
///
/// Implement this trait to wire the task tool into an agent registry.
/// Pass a prior session id in `resume` to continue that session; the
/// returned string is the actual session id used (a fresh one when
/// `resume` is `None`). `parent_session` is the conversation running the
/// task tool (from [`ToolContext::session_id`](gewu_core::types::ToolContext));
/// implementations may use it to track lineage. The factory itself holds no
/// per-session state.
#[async_trait]
pub trait AgentFactory: Send + Sync {
    async fn create(
        &self,
        subagent_type: &str,
        resume: Option<String>,
        parent_session: &str,
    ) -> Result<(Agent, String), Box<dyn Error + Send + Sync>>;
}

pub struct TaskTool {
    factory: Arc<dyn AgentFactory>,
    description: String,
    /// Subagent types this tool may launch; enforced in `execute`.
    allowed: Vec<String>,
}

#[derive(Deserialize)]
struct TaskArgs {
    #[allow(dead_code)]
    description: String,
    prompt: String,
    subagent_type: String,
    #[serde(default)]
    task_id: Option<String>,
}

fn schema() -> RootSchema {
    serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": {
            "description": { "type": "string", "description": "A short (3-5 words) description of the task" },
            "prompt": { "type": "string", "description": "The task for the agent to perform" },
            "subagent_type": { "type": "string", "description": "The type of specialized agent to use for this task" },
            "task_id": {
                "type": "string",
                "description": "This should only be set if you mean to resume a previous task (you can pass a prior task_id and the task will continue the same subagent session as before instead of creating a fresh one)"
            }
        },
        "required": ["description", "prompt", "subagent_type"]
    }))
    .unwrap()
}

impl TaskTool {
    /// Creates a task tool. `agents` is the (name, description) list
    /// rendered into the tool description. The same list is enforced as an
    /// allow-list: calls with any other `subagent_type` are rejected with an
    /// error result, so restricting the list (e.g. to read-only agents)
    /// actually restricts delegation.
    pub fn new(factory: Arc<dyn AgentFactory>, agents: Vec<(String, String)>) -> Self {
        let agent_list = agents
            .iter()
            .map(|(name, description)| format!("- \"{}\": {}", name, description))
            .collect::<Vec<_>>()
            .join("\n");

        let description = include_str!("prompts/task.txt").replace("{agents}", &agent_list);
        let allowed = agents.into_iter().map(|(name, _)| name).collect();
        Self {
            factory,
            description,
            allowed,
        }
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> RootSchema {
        schema()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: TaskArgs = serde_json::from_value(args)?;

        if !self.allowed.iter().any(|name| name == &input.subagent_type) {
            return Ok(ToolResult::new(
                "task",
                format!(
                    "Error: subagent type '{}' is not available. Available subagents: {}.",
                    input.subagent_type,
                    self.allowed.join(", ")
                ),
            ));
        }

        let (mut agent, session_id) = self
            .factory
            .create(&input.subagent_type, input.task_id, &ctx.session_id)
            .await
            .map_err(|e| format!("Failed to create agent: {}", e))?;

        use futures_util::StreamExt;
        let mut stream = Box::pin(agent.ask(&input.prompt));
        let mut result = String::new();

        while let Some(event_result) = stream.next().await {
            // Stop consuming the subagent stream once the parent turn has
            // been aborted; otherwise a canceled turn waits for the entire
            // subagent run to finish anyway.
            if ctx.is_aborted() {
                return Ok(ToolResult::new(
                    input.description,
                    "Error: task aborted because the parent turn was canceled.",
                ));
            }
            match event_result {
                Ok(event) => match event {
                    AgentEvent::ContentDelta { delta } => {
                        result.push_str(&delta);
                    }
                    AgentEvent::Finish { response } => {
                        result = response;
                    }
                    AgentEvent::Error { error } => {
                        return Err(format!("Agent execution failed: {}", error).into());
                    }
                    _ => {}
                },
                Err(e) => return Err(format!("Agent stream failed: {}", e).into()),
            }
        }

        Ok(ToolResult::new(
            input.description,
            format!(
                "task_id: {}\n\n<task_result>\n{}\n</task_result>",
                session_id, result
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream::BoxStream;
    use gewu_core::traits::LlmProvider;
    use gewu_core::types::{Message, ProviderEvent};
    use std::sync::Mutex;

    /// Streams canned provider events.
    struct MockLlm {
        events: Vec<ProviderEvent>,
        fail_stream: bool,
    }

    #[async_trait]
    impl LlmProvider for MockLlm {
        fn build(
            &self,
            _messages: &[Message],
            _tools: &[Arc<dyn Tool>],
        ) -> Result<serde_json::Value, Box<dyn Error + Send + Sync>> {
            Ok(serde_json::json!({ "model": "mock" }))
        }

        async fn send<'a>(
            &'a self,
            _request: serde_json::Value,
        ) -> Result<
            BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
            Box<dyn Error + Send + Sync>,
        > {
            if self.fail_stream {
                return Err("stream failed".into());
            }
            Ok(Box::pin(futures_util::stream::iter(
                self.events.clone().into_iter().map(Ok),
            )))
        }

        async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
            Ok(Vec::new())
        }
    }

    /// Records requested subagent types and spawns agents on MockLlm.
    struct MockFactory {
        events: Vec<ProviderEvent>,
        fail_stream: bool,
        fail_create: bool,
        requests: Mutex<Vec<(String, Option<String>, String)>>,
    }

    impl MockFactory {
        fn new(events: Vec<ProviderEvent>) -> Self {
            Self {
                events,
                fail_stream: false,
                fail_create: false,
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl AgentFactory for MockFactory {
        async fn create(
            &self,
            subagent_type: &str,
            resume: Option<String>,
            parent_session: &str,
        ) -> Result<(Agent, String), Box<dyn Error + Send + Sync>> {
            if self.fail_create {
                return Err("unknown agent type".into());
            }
            self.requests
                .lock()
                .unwrap()
                .push((subagent_type.to_string(), resume.clone(), parent_session.to_string()));
            let agent = Agent::builder()
                .llm(Arc::new(MockLlm {
                    events: self.events.clone(),
                    fail_stream: self.fail_stream,
                }))
                .build()
                .map_err(|e| -> Box<dyn Error + Send + Sync> { e.to_string().into() })?;
            Ok((agent, resume.unwrap_or_else(|| "fresh-session".to_string())))
        }
    }

    fn content_finish_events() -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::ContentDelta {
                delta: "working".to_string(),
            },
            ProviderEvent::Finish { reason: None },
        ]
    }

    fn make_tool(factory: Arc<MockFactory>) -> TaskTool {
        TaskTool::new(
            factory,
            vec![
                ("build".to_string(), "Builds the project".to_string()),
                ("explore".to_string(), "Explores the codebase".to_string()),
            ],
        )
    }

    fn args(description: &str, subagent_type: &str, task_id: Option<&str>) -> serde_json::Value {
        let mut value = serde_json::json!({
            "description": description,
            "prompt": "do the thing",
            "subagent_type": subagent_type,
        });
        if let Some(id) = task_id {
            value["task_id"] = serde_json::json!(id);
        }
        value
    }

    #[tokio::test]
    async fn task_runs_subagent_and_wraps_result() {
        let factory = Arc::new(MockFactory::new(content_finish_events()));
        let tool = make_tool(factory.clone());

        let result = tool
            .execute(
                args("Check files", "build", None),
                &ToolContext::new("s", "m"),
            )
            .await
            .expect("task should succeed");

        assert_eq!(result.title, "Check files");
        assert!(result.output.contains("task_id: fresh-session"));
        assert!(result.output.contains("<task_result>"));
        assert!(result.output.contains("working"));

        let requests = factory.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "build");
        assert_eq!(requests[0].1, None);
        assert_eq!(requests[0].2, "s");
    }

    #[tokio::test]
    async fn task_resume_reuses_provided_session_id() {
        let factory = Arc::new(MockFactory::new(content_finish_events()));
        let tool = make_tool(factory.clone());

        let result = tool
            .execute(
                args("Resume", "explore", Some("prior-1")),
                &ToolContext::new("s", "m"),
            )
            .await
            .expect("task should succeed");

        assert!(result.output.contains("task_id: prior-1"));
        let requests = factory.requests.lock().unwrap();
        assert_eq!(requests[0].0, "explore");
        assert_eq!(requests[0].1.as_deref(), Some("prior-1"));
        assert_eq!(requests[0].2, "s");
    }

    #[tokio::test]
    async fn task_reports_agent_execution_errors() {
        let events = vec![ProviderEvent::Error {
            error: "boom".to_string(),
        }];
        let tool = make_tool(Arc::new(MockFactory::new(events)));

        let error = tool
            .execute(args("Check", "build", None), &ToolContext::new("s", "m"))
            .await
            .expect_err("agent error should surface");

        assert!(error.to_string().contains("Agent execution failed: boom"));
    }

    #[tokio::test]
    async fn task_reports_agent_stream_errors() {
        let mut factory = MockFactory::new(content_finish_events());
        factory.fail_stream = true;
        let tool = make_tool(Arc::new(factory));

        let error = tool
            .execute(args("Check", "build", None), &ToolContext::new("s", "m"))
            .await
            .expect_err("stream error should surface");

        assert!(error.to_string().contains("Agent stream failed"));
    }

    #[tokio::test]
    async fn task_reports_factory_errors() {
        let mut factory = MockFactory::new(content_finish_events());
        factory.fail_create = true;
        let tool = make_tool(Arc::new(factory));

        // "build" is advertised, so the call reaches the factory and its
        // failure surfaces.
        let error = tool
            .execute(args("Check", "build", None), &ToolContext::new("s", "m"))
            .await
            .expect_err("factory error should surface");

        assert!(error.to_string().contains("Failed to create agent"));
    }

    #[tokio::test]
    async fn task_rejects_unadvertised_subagent_type() {
        let factory = Arc::new(MockFactory::new(content_finish_events()));
        let tool = make_tool(factory.clone());

        let result = tool
            .execute(args("Check", "general", None), &ToolContext::new("s", "m"))
            .await
            .expect("unadvertised type should be an error result, not a stream failure");

        assert!(result.output.contains("not available"));
        assert!(result.output.contains("explore"));
        // The factory must never be asked to create the denied agent.
        assert!(factory.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn task_description_lists_agents() {
        let tool = make_tool(Arc::new(MockFactory::new(content_finish_events())));
        let description = tool.description();
        assert!(description.contains("- \"build\": Builds the project"));
        assert!(description.contains("- \"explore\": Explores the codebase"));
    }
}
