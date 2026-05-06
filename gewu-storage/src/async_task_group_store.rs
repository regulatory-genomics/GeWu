use anyhow::Result;
use gewu_core::types::{TaskExecutionStatus, TaskRecord, TaskResultStatus, TaskStatus};
use sqlx::Row;
use sqlx::sqlite::SqlitePool;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncTaskGroup {
    pub group_id: String,
    pub parent_session_id: String,
    pub parent_message_id: String,
    pub expected_member_count: i64,
    pub status: String,
    pub injected_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct AsyncTaskGroupStore {
    pool: SqlitePool,
}

impl AsyncTaskGroupStore {
    pub async fn new(pool: SqlitePool) -> Result<Self> {
        // Schema lives in `gewu-storage/migrations/0003_async_task_groups.sql`.
        Ok(Self { pool })
    }

    pub async fn create_or_get_group(
        &self,
        parent_session_id: &str,
        parent_message_id: &str,
    ) -> Result<AsyncTaskGroup> {
        self.create_or_get_group_with_expected_count(parent_session_id, parent_message_id, 0)
            .await
    }

    pub async fn create_or_get_group_with_expected_count(
        &self,
        parent_session_id: &str,
        parent_message_id: &str,
        expected_member_count: i64,
    ) -> Result<AsyncTaskGroup> {
        let group_id = format!("async_group_{}", Uuid::new_v4());
        sqlx::query(
            r#"INSERT INTO async_task_groups (group_id, parent_session_id, parent_message_id, expected_member_count)
               VALUES (?, ?, ?, ?)
               ON CONFLICT(parent_session_id, parent_message_id)
               DO UPDATE SET
                   expected_member_count = MAX(async_task_groups.expected_member_count, excluded.expected_member_count),
                   updated_at = unixepoch()"#,
        )
        .bind(&group_id)
        .bind(parent_session_id)
        .bind(parent_message_id)
        .bind(expected_member_count.max(0))
        .execute(&self.pool)
        .await?;

        self.get_group_by_parent(parent_session_id, parent_message_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("group missing after upsert"))
    }

    pub async fn add_member(
        &self,
        group_id: &str,
        task_id: &str,
        child_session_id: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO async_task_group_members (group_id, task_id, child_session_id)
               VALUES (?, ?, ?)
               ON CONFLICT(group_id, task_id) DO UPDATE SET child_session_id = excluded.child_session_id"#,
        )
        .bind(group_id)
        .bind(task_id)
        .bind(child_session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_group_for_task(&self, task_id: &str) -> Result<Option<AsyncTaskGroup>> {
        let row = sqlx::query(
            r#"SELECT g.group_id, g.parent_session_id, g.parent_message_id,
                      g.expected_member_count, g.status, g.injected_at,
                      g.created_at, g.updated_at
               FROM async_task_groups g
               JOIN async_task_group_members m ON m.group_id = g.group_id
               WHERE m.task_id = ?"#,
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_group).transpose()
    }

    pub async fn list_group_members(&self, group_id: &str) -> Result<Vec<TaskRecord>> {
        let rows = sqlx::query(
            r#"SELECT t.task_id, t.specialist_id, t.prompt, t.status, t.execution_status,
                      t.result_status, t.attempt, t.block_reason, t.result_json,
                      t.finish_reason, t.error, t.parent_session_id, t.child_session_id,
                      t.root_run_id, t.started_at, t.finished_at
               FROM async_task_group_members m
               JOIN tasks t ON t.task_id = m.task_id
               WHERE m.group_id = ?
               ORDER BY COALESCE(t.started_at, 0), t.task_id"#,
        )
        .bind(group_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_task_record).collect()
    }

    pub async fn list_uninjected_groups(&self) -> Result<Vec<AsyncTaskGroup>> {
        let rows = sqlx::query(
            r#"SELECT group_id, parent_session_id, parent_message_id,
                      expected_member_count, status, injected_at, created_at, updated_at
               FROM async_task_groups WHERE injected_at IS NULL ORDER BY created_at"#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_group).collect()
    }

    pub async fn mark_injected(&self, group_id: &str) -> Result<bool> {
        let r = sqlx::query(
            r#"UPDATE async_task_groups
               SET status = 'completed', injected_at = unixepoch(), updated_at = unixepoch()
               WHERE group_id = ? AND injected_at IS NULL"#,
        )
        .bind(group_id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Variant of `mark_injected` that runs inside a caller-provided transaction.
    /// Used by callers that need to atomically pair the UPDATE with another insert
    /// (e.g. the BackgroundTaskCoordinator pairing this with a memory_messages
    /// INSERT to avoid the failure path described in C1/M6).
    pub async fn mark_injected_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        group_id: &str,
    ) -> Result<bool> {
        let r = sqlx::query(
            r#"UPDATE async_task_groups
               SET status = 'completed', injected_at = unixepoch(), updated_at = unixepoch()
               WHERE group_id = ? AND injected_at IS NULL"#,
        )
        .bind(group_id)
        .execute(&mut **tx)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    async fn get_group_by_parent(
        &self,
        parent_session_id: &str,
        parent_message_id: &str,
    ) -> Result<Option<AsyncTaskGroup>> {
        let row = sqlx::query(
            r#"SELECT group_id, parent_session_id, parent_message_id,
                      expected_member_count, status, injected_at, created_at, updated_at
               FROM async_task_groups
               WHERE parent_session_id = ? AND parent_message_id = ?"#,
        )
        .bind(parent_session_id)
        .bind(parent_message_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_group).transpose()
    }
}

fn row_to_group(row: sqlx::sqlite::SqliteRow) -> Result<AsyncTaskGroup> {
    Ok(AsyncTaskGroup {
        group_id: row.try_get("group_id")?,
        parent_session_id: row.try_get("parent_session_id")?,
        parent_message_id: row.try_get("parent_message_id")?,
        expected_member_count: row.try_get("expected_member_count")?,
        status: row.try_get("status")?,
        injected_at: row.try_get("injected_at")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn row_to_task_record(row: sqlx::sqlite::SqliteRow) -> Result<TaskRecord> {
    let status_str: String = row.try_get("status")?;
    let exec_str: String = row.try_get("execution_status")?;
    let result_str: String = row.try_get("result_status")?;
    Ok(TaskRecord {
        task_id: row.try_get("task_id")?,
        specialist_id: row.try_get("specialist_id")?,
        prompt: row.try_get("prompt")?,
        status: TaskStatus::from_str(&status_str).unwrap_or(TaskStatus::Failed),
        execution_status: TaskExecutionStatus::from_str(&exec_str)
            .unwrap_or(TaskExecutionStatus::Interrupted),
        result_status: TaskResultStatus::from_str(&result_str)
            .unwrap_or(TaskResultStatus::NeedsReview),
        attempt: row.try_get::<i32, _>("attempt")? as u32,
        block_reason: row.try_get("block_reason")?,
        result_json: row.try_get("result_json")?,
        finish_reason: row.try_get("finish_reason")?,
        error: row.try_get("error")?,
        parent_session_id: row.try_get("parent_session_id")?,
        child_session_id: row.try_get("child_session_id")?,
        root_run_id: row.try_get("root_run_id")?,
        started_at: row.try_get("started_at")?,
        finished_at: row.try_get("finished_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn make_pool() -> Result<sqlx::sqlite::SqlitePool> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        crate::run_migrations(&pool).await?;
        Ok(pool)
    }

    #[tokio::test]
    async fn create_or_get_group_reuses_parent_turn() -> Result<()> {
        let pool = make_pool().await?;
        let store = AsyncTaskGroupStore::new(pool).await?;
        let first = store.create_or_get_group("parent", "msg").await?;
        let second = store.create_or_get_group("parent", "msg").await?;
        assert_eq!(first.group_id, second.group_id);
        Ok(())
    }

    #[tokio::test]
    async fn create_or_get_group_keeps_largest_expected_count() -> Result<()> {
        let pool = make_pool().await?;
        let store = AsyncTaskGroupStore::new(pool).await?;
        let first = store
            .create_or_get_group_with_expected_count("parent", "msg", 3)
            .await?;
        let second = store
            .create_or_get_group_with_expected_count("parent", "msg", 1)
            .await?;

        assert_eq!(first.group_id, second.group_id);
        assert_eq!(second.expected_member_count, 3);
        Ok(())
    }

    #[tokio::test]
    async fn mark_injected_is_idempotent() -> Result<()> {
        let pool = make_pool().await?;
        let store = AsyncTaskGroupStore::new(pool).await?;
        let group = store.create_or_get_group("parent", "msg").await?;
        assert!(store.mark_injected(&group.group_id).await?);
        assert!(!store.mark_injected(&group.group_id).await?);
        Ok(())
    }
}
