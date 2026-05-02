use anyhow::Result;
use gewu_core::traits::Memory;
use gewu_core::types::{Message, TaskExecutionStatus, TaskRecord};
use gewu_storage::{AsyncTaskGroup, AsyncTaskGroupStore, SqliteMemory};
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct BackgroundTaskCoordinator {
    pool: SqlitePool,
}

#[derive(Debug, Clone)]
pub struct TaskSettledEvent {
    pub task_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackgroundGroupCompletedEvent {
    pub group_id: String,
    pub parent_session_id: String,
    pub parent_message_id: String,
    pub summary: String,
}

static BG_EVENTS: std::sync::OnceLock<broadcast::Sender<BackgroundGroupCompletedEvent>> =
    std::sync::OnceLock::new();

pub fn subscribe_background_events() -> broadcast::Receiver<BackgroundGroupCompletedEvent> {
    bg_sender().subscribe()
}

fn bg_sender() -> &'static broadcast::Sender<BackgroundGroupCompletedEvent> {
    BG_EVENTS.get_or_init(|| broadcast::channel(256).0)
}

impl BackgroundTaskCoordinator {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn register_async_task(
        &self,
        parent_session_id: &str,
        parent_message_id: &str,
        task_id: &str,
        child_session_id: &str,
    ) -> Result<AsyncTaskGroup> {
        let store = AsyncTaskGroupStore::new(self.pool.clone()).await?;
        let group = store
            .create_or_get_group(parent_session_id, parent_message_id)
            .await?;
        store
            .add_member(&group.group_id, task_id, child_session_id)
            .await?;
        Ok(group)
    }

    pub async fn on_task_settled(&self, event: TaskSettledEvent) -> Result<bool> {
        let store = AsyncTaskGroupStore::new(self.pool.clone()).await?;
        let Some(group) = store.get_group_for_task(&event.task_id).await? else {
            return Ok(false);
        };
        self.maybe_inject(&store, group).await
    }

    pub async fn reconcile_uninjected_groups(&self) -> Result<usize> {
        let store = AsyncTaskGroupStore::new(self.pool.clone()).await?;
        let groups = store.list_uninjected_groups().await?;
        let mut count = 0usize;
        for group in groups {
            if self.maybe_inject(&store, group).await? {
                count += 1;
            }
        }
        Ok(count)
    }

    async fn maybe_inject(
        &self,
        store: &AsyncTaskGroupStore,
        group: AsyncTaskGroup,
    ) -> Result<bool> {
        if group.injected_at.is_some() {
            return Ok(false);
        }
        let members = store.list_group_members(&group.group_id).await?;
        if members.is_empty() || members.iter().any(|t| !is_settled(&t.execution_status)) {
            return Ok(false);
        }
        if !store.mark_injected(&group.group_id).await? {
            return Ok(false);
        }

        let message = build_completion_message(&group, &members);
        let mut memory =
            SqliteMemory::new(self.pool.clone(), group.parent_session_id.clone()).await?;
        memory.add_message(message).await;

        let summary = build_summary_line(&members);
        let _ = bg_sender().send(BackgroundGroupCompletedEvent {
            group_id: group.group_id,
            parent_session_id: group.parent_session_id,
            parent_message_id: group.parent_message_id,
            summary,
        });
        Ok(true)
    }
}

pub fn is_settled(status: &TaskExecutionStatus) -> bool {
    matches!(
        status,
        TaskExecutionStatus::Completed
            | TaskExecutionStatus::Failed
            | TaskExecutionStatus::Canceled
            | TaskExecutionStatus::Timeout
            | TaskExecutionStatus::Interrupted
    )
}

fn build_summary_line(tasks: &[TaskRecord]) -> String {
    let ok = tasks
        .iter()
        .filter(|t| t.execution_status == TaskExecutionStatus::Completed)
        .count();
    let fail = tasks.len().saturating_sub(ok);
    if fail == 0 {
        format!("Background tasks completed: {} succeeded.", ok)
    } else {
        format!(
            "Background tasks completed: {} succeeded, {} failed.",
            ok, fail
        )
    }
}

/// Build a System message with human-readable summary + each child's result excerpt.
/// NOT an Assistant message — the agent can read it but it won't render as chat.
pub fn build_completion_message(group: &AsyncTaskGroup, tasks: &[TaskRecord]) -> Message {
    let summary = build_summary_line(tasks);
    let lines = tasks
        .iter()
        .enumerate()
        .map(|(i, task)| {
            let title = task_title(task);
            let status_line = format!(
                "{}. {} — {} / {}",
                i + 1,
                title,
                task.execution_status.as_str(),
                task.result_status.as_str()
            );
            let mut parts = vec![status_line];
            if let Some(err) = task.error.as_deref().or(task.block_reason.as_deref()) {
                if !err.trim().is_empty() {
                    parts.push(format!("   Error: {}", truncate(err.trim(), 240)));
                }
            }
            if let Some(excerpt) = task_result_excerpt(task) {
                parts.push(indent(&excerpt));
            }
            parts.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let content = format!(
        "[background_tasks.completed] group_id={}\n{}\n\n{}",
        group.group_id, summary, lines
    );
    Message::System { content }
}

fn task_title(task: &TaskRecord) -> String {
    let first = task.prompt.lines().next().unwrap_or_default().trim();
    if first.is_empty() {
        task.specialist_id.clone()
    } else {
        truncate(first, 80)
    }
}

fn task_result_excerpt(task: &TaskRecord) -> Option<String> {
    let raw = task.result_json.as_deref()?;
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let text = match parsed {
        Value::String(s) => s,
        Value::Object(ref map) => map
            .get("summary")
            .or_else(|| map.get("output"))
            .or_else(|| map.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        other => other.to_string(),
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(truncate(trimmed, 1200))
    }
}

fn truncate(value: &str, max: usize) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    for _ in 0..max {
        let Some(ch) = chars.next() else { return out };
        out.push(ch);
    }
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

fn indent(value: &str) -> String {
    value
        .lines()
        .map(|l| format!("   {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_core::types::{TaskResultStatus, TaskStatus};

    #[test]
    fn pending_and_running_are_not_settled() {
        assert!(!is_settled(&TaskExecutionStatus::Pending));
        assert!(!is_settled(&TaskExecutionStatus::Running));
        assert!(is_settled(&TaskExecutionStatus::Completed));
        assert!(is_settled(&TaskExecutionStatus::Failed));
    }

    #[test]
    fn completion_message_is_system_not_assistant() {
        let group = test_group();
        let tasks = vec![test_record(
            "a",
            TaskExecutionStatus::Completed,
            TaskResultStatus::Accepted,
        )];
        let msg = build_completion_message(&group, &tasks);
        match msg {
            Message::System { ref content } => {
                assert!(content.contains("background_tasks.completed"));
                assert!(content.contains("1 succeeded"));
                assert!(!content.contains("```json"));
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn completion_message_includes_result_excerpt() {
        let group = test_group();
        let mut task = test_record(
            "a",
            TaskExecutionStatus::Completed,
            TaskResultStatus::Accepted,
        );
        task.result_json = Some(serde_json::to_string("Found 13 test files.").unwrap());
        let msg = build_completion_message(&group, &[task]);
        match msg {
            Message::System { content } => assert!(content.contains("Found 13 test files")),
            _ => panic!("expected System"),
        }
    }

    fn test_group() -> AsyncTaskGroup {
        AsyncTaskGroup {
            group_id: "g1".into(),
            parent_session_id: "parent".into(),
            parent_message_id: "msg".into(),
            status: "running".into(),
            injected_at: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn test_record(id: &str, exec: TaskExecutionStatus, result: TaskResultStatus) -> TaskRecord {
        TaskRecord {
            task_id: id.into(),
            specialist_id: "explore".into(),
            prompt: format!("Task {id}"),
            status: TaskStatus::Succeeded,
            execution_status: exec,
            result_status: result,
            attempt: 1,
            block_reason: None,
            result_json: None,
            finish_reason: Some("stop".into()),
            error: None,
            parent_session_id: Some("parent".into()),
            child_session_id: Some(id.into()),
            root_run_id: None,
            started_at: Some(1),
            finished_at: Some(2),
        }
    }
}
