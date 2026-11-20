use crate::project::RunningTask;
use crate::{Lab, ProjectUpdate, Task, TaskLoop};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use gewu_core::agent::AgentBuilder;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::{JsonSchema, schema::RootSchema};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::error::Error;
use std::sync::{Arc, Mutex};

/// Runs one delegated conversation to completion: a project agent for
/// `ask_project_agent` or a task agent for `ask_task_agent`. The runner owns
/// agent construction; the tool forwards the message and awaits the reply.
pub type AgentRunner = Arc<
    dyn Fn(String, String) -> futures::future::BoxFuture<'static, anyhow::Result<String>>
        + Send
        + Sync,
>;

/// Builds an agent for a delegated conversation, keyed by project or task id.
pub type AgentBuilderFactory = Arc<dyn Fn(&str) -> Result<AgentBuilder> + Send + Sync>;

/// Builds the task loop that executes a started task.
pub type TaskLoopFactory = Arc<dyn Fn(&Task) -> Result<TaskLoop> + Send + Sync>;

fn task_runner(factory: AgentBuilderFactory) -> AgentRunner {
    Arc::new(move |task_id, message| {
        let factory = factory.clone();
        Box::pin(async move {
            let agent = factory(&task_id)?.build()?;
            TaskLoop::new(agent).run(&message).await
        })
    })
}

/// Builds a project's task board tool with the capabilities this coordinator
/// supports: running tasks (`start_task`) and delegating to task agents.
fn build_board(
    lab: Arc<Mutex<Lab>>,
    project_id: &str,
    task_loops: Option<&TaskLoopFactory>,
    tasks: Option<&AgentBuilderFactory>,
) -> ProjectTaskBoardTool {
    let mut board = ProjectTaskBoardTool::new(lab, project_id);
    if let Some(factory) = task_loops {
        board = board.with_task_loops(factory.clone());
    }
    if let Some(factory) = tasks {
        board = board.with_task_runner(task_runner(factory.clone()));
    }
    board
}

pub struct LabCoordinator {
    lab: Arc<Mutex<Lab>>,
    project_builder: Option<AgentBuilderFactory>,
    task_builder: Option<AgentBuilderFactory>,
    task_loop_factory: Option<TaskLoopFactory>,
}

impl LabCoordinator {
    pub fn new(lab: Arc<Mutex<Lab>>) -> Self {
        Self {
            lab,
            project_builder: None,
            task_builder: None,
            task_loop_factory: None,
        }
    }

    /// Sends project messages synchronously: each `ask_project_agent` runs a
    /// fresh project agent (built by `factory`) and waits for its reply.
    pub fn with_sync_projects(mut self, factory: AgentBuilderFactory) -> Self {
        self.project_builder = Some(factory);
        self
    }

    /// Lets project agents delegate to task agents synchronously via
    /// `ask_task_agent`, building each task agent with `factory`.
    pub fn with_sync_tasks(mut self, factory: AgentBuilderFactory) -> Self {
        self.task_builder = Some(factory);
        self
    }

    /// Lets project agents start task execution synchronously via
    /// `start_task`, building each task loop with `factory`.
    pub fn with_task_dispatch(mut self, factory: TaskLoopFactory) -> Self {
        self.task_loop_factory = Some(factory);
        self
    }

    /// The task board tool for a project, wired with this coordinator's
    /// execution and delegation capabilities.
    fn board(&self, project_id: &str) -> ProjectTaskBoardTool {
        build_board(
            self.lab.clone(),
            project_id,
            self.task_loop_factory.as_ref(),
            self.task_builder.as_ref(),
        )
    }

    pub async fn handle_human_message(
        &mut self,
        builder: AgentBuilder,
        message: &str,
    ) -> Result<String> {
        let mut tool = LabProjectTool::new(self.lab.clone());
        if let Some(factory) = &self.project_builder {
            let lab = self.lab.clone();
            let factory = factory.clone();
            let loops = self.task_loop_factory.clone();
            let tasks = self.task_builder.clone();
            tool = tool.with_project_runner(Arc::new(move |project_id, message| {
                let lab = lab.clone();
                let factory = factory.clone();
                let loops = loops.clone();
                let tasks = tasks.clone();
                Box::pin(async move {
                    let board = build_board(lab, &project_id, loops.as_ref(), tasks.as_ref());
                    let agent = factory(&project_id)?
                        .with_tools(vec![Arc::new(board)])
                        .build()?;
                    TaskLoop::new(agent).run(&message).await
                })
            }));
        }
        let agent = builder.with_tools(vec![Arc::new(tool)]).build()?;
        TaskLoop::new(agent).run(message).await
    }

    pub async fn handle_project_message(
        &mut self,
        project_id: &str,
        builder: AgentBuilder,
    ) -> Result<Option<String>> {
        let tool = self.board(project_id);
        let agent = builder.with_tools(vec![Arc::new(tool)]).build()?;
        let message = self
            .lab
            .lock()
            .map_err(|_| anyhow!("lab lock poisoned"))?
            .take_project_message(project_id)?;
        match message {
            Some(message) => TaskLoop::new(agent).run(&message).await.map(Some),
            None => Ok(None),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum LabAction {
    /// List every project with its id, name, goal, and wiki path.
    ListProjects,
    /// Create a project; the lab assigns its unique id.
    CreateProject { name: String, goal: String },
    /// Send a message to a project agent and wait for its response.
    AskProjectAgent { id: String, message: String },
}

pub struct LabProjectTool {
    lab: Arc<Mutex<Lab>>,
    project_runner: Option<AgentRunner>,
}

impl LabProjectTool {
    pub fn new(lab: Arc<Mutex<Lab>>) -> Self {
        Self {
            lab,
            project_runner: None,
        }
    }

    pub fn with_project_runner(mut self, runner: AgentRunner) -> Self {
        self.project_runner = Some(runner);
        self
    }
}

#[async_trait]
impl Tool for LabProjectTool {
    fn name(&self) -> &str {
        "lab_project"
    }

    fn description(&self) -> &str {
        "Manage lab projects. Actions: list_projects; create_project from a name and goal (a unique id is assigned); ask_project_agent sends a message to a project agent by id and waits for its response."
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(LabAction)
    }

    async fn execute(
        &self,
        args: Value,
        _: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let action = serde_json::from_value(args)?;
        // The lab lock is only held inside this block: delegated sends must
        // never hold it across the runner's await.
        enum Delegated {
            Done(Value),
            Ask(AgentRunner, String, String),
        }
        let delegated = {
            let mut lab = self.lab.lock().map_err(|_| anyhow!("lab lock poisoned"))?;
            match action {
                LabAction::ListProjects => {
                    let projects: Vec<_> = lab.projects()
                        .map(|(id, project)| json!({"id": id, "name": project.name(), "goal": project.goal(), "wiki": format!("wikis/{id}/")})).collect();
                    Delegated::Done(json!({"projects": projects}))
                }
                LabAction::CreateProject { name, goal } => {
                    let id = lab.create_project(&name, &goal)?;
                    Delegated::Done(json!({"id": id, "status": "created"}))
                }
                LabAction::AskProjectAgent { id, message } => {
                    lab.project(&id)
                        .ok_or_else(|| anyhow!("unknown project: {id}"))?;
                    match self.project_runner.clone() {
                        Some(runner) => Delegated::Ask(runner, id, message),
                        None => {
                            lab.send_project_message(&id, message)?;
                            Delegated::Done(json!({"id": id, "status": "queued"}))
                        }
                    }
                }
            }
        };
        let output = match delegated {
            Delegated::Done(output) => output,
            Delegated::Ask(runner, id, message) => {
                let reply = runner(id.clone(), message).await?;
                json!({"id": id, "status": "replied", "reply": reply})
            }
        };
        Ok(ToolResult::new(self.name(), output.to_string()))
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum BoardAction {
    /// Start a task: fails unless the task is Pending, runs its task loop,
    /// and records the attempt for review.
    StartTask {
        task_id: String,
    },
    ListTask {},
    /// Create a task; optionally depend on existing tasks by id.
    CreateTask {
        name: String,
        #[serde(default)]
        depends_on: Vec<String>,
    },
    Add {
        task_id: String,
        name: String,
        deliverable: String,
        #[serde(default)]
        dependencies: BTreeSet<String>,
    },
    Revise {
        task_id: String,
        name: String,
        deliverable: String,
        dependencies: BTreeSet<String>,
    },
    /// Send a message to a task agent and wait for its response.
    AskTaskAgent {
        task_id: String,
        message: String,
    },
}

pub struct ProjectTaskBoardTool {
    lab: Arc<Mutex<Lab>>,
    project_id: String,
    task_runner: Option<AgentRunner>,
    task_loops: Option<TaskLoopFactory>,
}

impl ProjectTaskBoardTool {
    pub fn new(lab: Arc<Mutex<Lab>>, project_id: impl Into<String>) -> Self {
        Self {
            lab,
            project_id: project_id.into(),
            task_runner: None,
            task_loops: None,
        }
    }

    pub fn with_task_loops(mut self, factory: TaskLoopFactory) -> Self {
        self.task_loops = Some(factory);
        self
    }

    pub fn with_task_runner(mut self, runner: AgentRunner) -> Self {
        self.task_runner = Some(runner);
        self
    }

    /// Claims the task, runs its task loop without holding the lab lock, and
    /// records the attempt. A dropped run future marks the task blocked.
    pub(crate) async fn start_task(
        &self,
        task_id: &str,
    ) -> Result<Value, Box<dyn Error + Send + Sync>> {
        let factory = self
            .task_loops
            .clone()
            .ok_or_else(|| anyhow!("no task loop factory is configured"))?;
        // Phase 1: claim the task under the lock.
        let run = {
            let mut lab = self.lab.lock().map_err(|_| anyhow!("lab lock poisoned"))?;
            let project = lab
                .project_mut(&self.project_id)
                .ok_or_else(|| anyhow!("unknown project: {}", self.project_id))?;
            let (attempt, prompt) = project.begin_attempt(task_id)?;
            let task_loop = {
                let task = project.task(task_id).expect("task begun above");
                factory(task)?
            };
            (
                RunningTask::new(self.lab.clone(), &self.project_id, task_id),
                task_loop,
                attempt,
                prompt,
            )
        };
        // Phase 2: run without the lock.
        let (mut guard, task_loop, attempt, prompt) = run;
        let outcome = task_loop.run(&prompt).await;
        guard.disarm();
        // Phase 3: record the attempt under the lock.
        let mut lab = self.lab.lock().map_err(|_| anyhow!("lab lock poisoned"))?;
        let attempt = lab
            .project_mut(&self.project_id)
            .expect("project checked in phase 1")
            .finish_attempt(task_id, attempt, outcome)?;
        let status = if attempt.feedback.is_none() {
            "awaiting_review"
        } else {
            "blocked"
        };
        Ok(json!({
            "task_id": task_id,
            "attempt_id": attempt.id,
            "status": status,
            "summary": attempt.summary,
        }))
    }
}

#[async_trait]
impl Tool for ProjectTaskBoardTool {
    fn name(&self) -> &str {
        "project_task_board"
    }

    fn description(&self) -> &str {
        "Manage this project's task board. Actions: start_task runs a pending task and records the attempt for review (fails unless Pending); list_task; create_task with a name and optional depends_on list; add or revise a pending task with explicit dependencies; ask_task_agent sends a message to a task agent and waits for its response."
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(BoardAction)
    }

    async fn execute(
        &self,
        args: Value,
        _: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let action: BoardAction = serde_json::from_value(args)?;
        if let BoardAction::StartTask { task_id } = action {
            let output = self.start_task(&task_id).await?;
            return Ok(ToolResult::new(self.name(), output.to_string()));
        }
        // The lab lock is only held inside this block: delegated sends must
        // never hold it across the runner's await.
        enum Delegated {
            Done(Value),
            Ask(AgentRunner, String, String),
        }
        let delegated = {
            let mut lab = self.lab.lock().map_err(|_| anyhow!("lab lock poisoned"))?;
            match action {
                BoardAction::StartTask { .. } => unreachable!("handled above"),
                BoardAction::ListTask {} => {
                    let project = lab
                        .project(&self.project_id)
                        .ok_or_else(|| anyhow!("unknown project: {}", self.project_id))?;
                    let tasks: Vec<_> = project.task_board().map(|(task, dependencies)| {
                        json!({"task_id": task.id, "name": task.name, "deliverable": task.deliverable, "status": format!("{:?}", task.status), "dependencies": dependencies})
                    }).collect();
                    Delegated::Done(json!({"project_id": self.project_id, "tasks": tasks}))
                }
                BoardAction::CreateTask { name, depends_on } => {
                    if name.trim().is_empty() {
                        return Err(anyhow!("task name must not be empty").into());
                    }
                    let task_count = lab
                        .project(&self.project_id)
                        .ok_or_else(|| anyhow!("unknown project: {}", self.project_id))?
                        .task_count();
                    let task_id = format!("task-{}", task_count + 1);
                    let deliverable = name.trim().to_owned();
                    lab.apply_update(
                        &self.project_id,
                        ProjectUpdate::Add {
                            task: Task::new(&task_id, name, deliverable),
                            dependencies: depends_on.into_iter().collect(),
                        },
                    )?;
                    Delegated::Done(json!({"task_id": task_id, "status": "added"}))
                }
                BoardAction::Add {
                    task_id,
                    name,
                    deliverable,
                    dependencies,
                } => {
                    if task_id.trim().is_empty() {
                        return Err(anyhow!("task ID must not be empty").into());
                    }
                    lab.apply_update(
                        &self.project_id,
                        ProjectUpdate::Add {
                            task: Task::new(&task_id, name, deliverable),
                            dependencies,
                        },
                    )?;
                    Delegated::Done(json!({"task_id": task_id, "status": "added"}))
                }
                BoardAction::Revise {
                    task_id,
                    name,
                    deliverable,
                    dependencies,
                } => {
                    lab.apply_update(
                        &self.project_id,
                        ProjectUpdate::Revise {
                            task_id: task_id.clone(),
                            name,
                            deliverable,
                            dependencies,
                        },
                    )?;
                    Delegated::Done(json!({"task_id": task_id, "status": "revised"}))
                }
                BoardAction::AskTaskAgent { task_id, message } => {
                    lab.project(&self.project_id)
                        .ok_or_else(|| anyhow!("unknown project: {}", self.project_id))?
                        .task(&task_id)
                        .ok_or_else(|| anyhow!("unknown task: {task_id}"))?;
                    let runner = self
                        .task_runner
                        .clone()
                        .ok_or_else(|| anyhow!("no task runner is configured"))?;
                    Delegated::Ask(runner, task_id, message)
                }
            }
        };
        let output = match delegated {
            Delegated::Done(output) => output,
            Delegated::Ask(runner, task_id, message) => {
                let reply = runner(task_id.clone(), message).await?;
                json!({"task_id": task_id, "status": "replied", "reply": reply})
            }
        };
        Ok(ToolResult::new(self.name(), output.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_loop::tests::scripted;
    use crate::{Project, Task};
    use futures::StreamExt;
    use gewu_core::agent::Agent;
    use gewu_core::types::{Message, ProviderEvent};

    /// A provider whose stream never finishes: used to test dropped runs.
    struct HangingProvider;

    #[async_trait::async_trait]
    impl gewu_core::traits::LlmProvider for HangingProvider {
        fn build(
            &self,
            _: &[Message],
            _: &[Arc<dyn gewu_core::traits::Tool>],
        ) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Value::Null)
        }

        async fn send<'a>(
            &'a self,
            _: Value,
        ) -> std::result::Result<
            futures::stream::BoxStream<
                'a,
                std::result::Result<
                    gewu_core::types::ProviderEvent,
                    Box<dyn std::error::Error + Send + Sync>,
                >,
            >,
            Box<dyn std::error::Error + Send + Sync>,
        > {
            Ok(futures::stream::pending().boxed())
        }

        async fn list_models(
            &self,
        ) -> std::result::Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Vec::new())
        }
    }

    /// A provider that replays scripted events, so tests can drive tool calls.
    fn scripted_calls(
        responses: Vec<Vec<ProviderEvent>>,
    ) -> Arc<dyn gewu_core::traits::LlmProvider> {
        Arc::new(ScriptedCalls(Mutex::new(responses.into_iter())))
    }

    struct ScriptedCalls(Mutex<std::vec::IntoIter<Vec<ProviderEvent>>>);

    #[async_trait::async_trait]
    impl gewu_core::traits::LlmProvider for ScriptedCalls {
        fn build(
            &self,
            _: &[Message],
            _: &[Arc<dyn gewu_core::traits::Tool>],
        ) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Value::Null)
        }

        async fn send<'a>(
            &'a self,
            _: Value,
        ) -> std::result::Result<
            futures::stream::BoxStream<
                'a,
                std::result::Result<
                    gewu_core::types::ProviderEvent,
                    Box<dyn std::error::Error + Send + Sync>,
                >,
            >,
            Box<dyn std::error::Error + Send + Sync>,
        > {
            let events = self.0.lock().unwrap().next().ok_or("no scripted call")?;
            Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
        }

        async fn list_models(
            &self,
        ) -> std::result::Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Vec::new())
        }
    }

    /// A response that calls one tool, then a plain text reply.
    fn tool_call(name: &str, arguments: &str) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::ToolCallStart {
                id: "call".into(),
                name: name.into(),
            },
            ProviderEvent::ToolCallDelta {
                id: "call".into(),
                delta: arguments.into(),
            },
            ProviderEvent::Finish {
                reason: Some("tool_calls".into()),
            },
        ]
    }

    fn said(text: &str) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::ContentDelta { delta: text.into() },
            ProviderEvent::Finish { reason: None },
        ]
    }

    fn setup() -> (Arc<Mutex<Lab>>, ToolContext) {
        let mut lab = Lab::new();
        let mut project = Project::new();
        project
            .add_task(Task::new("t1", "Explore", "Notes"), BTreeSet::new())
            .unwrap();
        lab.add_project("p", project).unwrap();
        let lab = Arc::new(Mutex::new(lab));
        (lab, ToolContext::new("test", "message"))
    }

    #[tokio::test]
    async fn ask_project_agent_waits_for_the_project_reply() {
        let (lab, ctx) = setup();
        let factory: AgentBuilderFactory =
            Arc::new(|_| Ok(Agent::builder().llm(scripted(vec!["project done"]))));
        let lab_for_runner = lab.clone();
        let factory_for_runner = factory.clone();
        let runner: AgentRunner = Arc::new(move |project_id, message| {
            let lab = lab_for_runner.clone();
            let factory = factory_for_runner.clone();
            Box::pin(async move {
                let board = ProjectTaskBoardTool::new(lab, &project_id);
                let agent = factory(&project_id)?
                    .with_tools(vec![Arc::new(board)])
                    .build()?;
                TaskLoop::new(agent).run(&message).await
            })
        });
        let tool = LabProjectTool::new(lab).with_project_runner(runner);
        let result = tool
            .execute(
                json!({"action": "ask_project_agent", "id": "p", "message": "status?"}),
                &ctx,
            )
            .await
            .unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["status"], "replied");
        assert_eq!(output["reply"], "project done");
    }

    #[tokio::test]
    async fn ask_project_agent_queues_without_a_runner_and_errors_on_unknown_ids() {
        let (lab, ctx) = setup();
        let tool = LabProjectTool::new(lab.clone());
        let result = tool
            .execute(
                json!({"action": "ask_project_agent", "id": "p", "message": "hi"}),
                &ctx,
            )
            .await
            .unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["status"], "queued");
        assert!(
            tool.execute(
                json!({"action": "ask_project_agent", "id": "missing", "message": "hi"}),
                &ctx
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn ask_task_agent_waits_for_the_task_reply_and_validates_ids() {
        let (lab, ctx) = setup();
        let runner: AgentRunner = Arc::new(|task_id, message| {
            Box::pin(async move { Ok(format!("{task_id}: {message}")) })
        });
        let tool = ProjectTaskBoardTool::new(lab, "p").with_task_runner(runner);
        let result = tool
            .execute(
                json!({"action": "ask_task_agent", "task_id": "t1", "message": "go"}),
                &ctx,
            )
            .await
            .unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["status"], "replied");
        assert_eq!(output["reply"], "t1: go");
        assert!(
            tool.execute(
                json!({"action": "ask_task_agent", "task_id": "missing", "message": "go"}),
                &ctx
            )
            .await
            .is_err()
        );
    }
    #[tokio::test]
    async fn start_task_runs_the_task_and_records_the_attempt() {
        let (lab, ctx) = setup();
        let factory: TaskLoopFactory = Arc::new(|task| {
            let reply = if task.id == "t1" { "done" } else { "second" };
            Ok(TaskLoop::new(
                Agent::builder().llm(scripted(vec![reply])).build()?,
            ))
        });
        let tool = ProjectTaskBoardTool::new(lab.clone(), "p").with_task_loops(factory);
        let result = tool
            .execute(json!({"action": "start_task", "task_id": "t1"}), &ctx)
            .await
            .unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["status"], "awaiting_review");
        assert_eq!(output["attempt_id"], "t1:1");
        assert_eq!(output["summary"], "done");
        assert_eq!(
            lab.lock()
                .unwrap()
                .project("p")
                .unwrap()
                .task("t1")
                .unwrap()
                .status,
            crate::TaskStatus::AwaitingReview
        );
        assert!(
            tool.execute(json!({"action": "start_task", "task_id": "t1"}), &ctx)
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({"action": "start_task", "task_id": "missing"}), &ctx)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn start_task_requires_a_task_loop_factory() {
        let (lab, ctx) = setup();
        let tool = ProjectTaskBoardTool::new(lab, "p");
        assert!(
            tool.execute(json!({"action": "start_task", "task_id": "t1"}), &ctx)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn failed_start_task_run_is_recorded_as_blocked() {
        let (lab, ctx) = setup();
        let tool = ProjectTaskBoardTool::new(lab.clone(), "p").with_task_loops(Arc::new(|_| {
            Ok(TaskLoop::new(
                Agent::builder().llm(scripted(vec![])).build()?,
            ))
        }));
        let result = tool
            .execute(json!({"action": "start_task", "task_id": "t1"}), &ctx)
            .await
            .unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["status"], "blocked");
        assert_eq!(
            lab.lock()
                .unwrap()
                .project("p")
                .unwrap()
                .task("t1")
                .unwrap()
                .status,
            crate::TaskStatus::Blocked
        );
    }

    #[tokio::test]
    async fn dropped_start_task_run_marks_the_task_blocked() {
        let (lab, ctx) = setup();
        let tool = ProjectTaskBoardTool::new(lab.clone(), "p").with_task_loops(Arc::new(|_| {
            Ok(TaskLoop::new(
                Agent::builder().llm(Arc::new(HangingProvider)).build()?,
            ))
        }));
        let mut run =
            Box::pin(tool.execute(json!({"action": "start_task", "task_id": "t1"}), &ctx));
        assert!(futures::poll!(&mut run).is_pending());
        drop(run);
        let task = lab
            .lock()
            .unwrap()
            .project("p")
            .unwrap()
            .task("t1")
            .cloned()
            .unwrap();
        assert_eq!(task.status, crate::TaskStatus::Blocked);
    }
    #[tokio::test]
    async fn human_message_wires_task_execution_into_project_agents() {
        let (lab, _) = setup();
        let project_agent = scripted_calls(vec![
            tool_call(
                "project_task_board",
                r#"{"action":"start_task","task_id":"t1"}"#,
            ),
            said("started"),
        ]);
        let lab_agent = scripted_calls(vec![
            tool_call(
                "lab_project",
                r#"{"action":"ask_project_agent","id":"p","message":"start t1"}"#,
            ),
            said("task started"),
        ]);
        let mut coordinator = LabCoordinator::new(lab.clone())
            .with_sync_projects(Arc::new(move |_| {
                Ok(Agent::builder().llm(project_agent.clone()))
            }))
            .with_sync_tasks(Arc::new(|_| Ok(Agent::builder().llm(scripted(vec![])))))
            .with_task_dispatch(Arc::new(|_| {
                Ok(TaskLoop::new(
                    Agent::builder().llm(scripted(vec!["done"])).build()?,
                ))
            }));
        let reply = coordinator
            .handle_human_message(Agent::builder().llm(lab_agent), "start t1")
            .await
            .unwrap();
        assert_eq!(reply, "task started");
        let task = lab
            .lock()
            .unwrap()
            .project("p")
            .unwrap()
            .task("t1")
            .unwrap()
            .clone();
        assert_eq!(task.status, crate::TaskStatus::AwaitingReview);
    }
}
