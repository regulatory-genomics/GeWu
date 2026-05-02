use anyhow::{Context, Result};
use gewu_core::types::{TaskExecutionStatus, TaskRecord, TaskResultStatus, TaskStatus};
use serde_json::Value;
use sqlx::Row;
use sqlx::sqlite::SqlitePool;

/// Durable task store backed by SQLite.
pub struct TaskStore {
    pool: SqlitePool,
}

impl TaskStore {
    pub async fn new(pool: SqlitePool) -> Result<Self> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS tasks (
                task_id TEXT PRIMARY KEY,
                specialist_id TEXT NOT NULL,
                prompt TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'ready',
                execution_status TEXT NOT NULL DEFAULT 'pending',
                result_status TEXT NOT NULL DEFAULT 'pending',
                attempt INTEGER NOT NULL DEFAULT 0,
                block_reason TEXT,
                result_json TEXT,
                finish_reason TEXT,
                error TEXT,
                parent_session_id TEXT,
                child_session_id TEXT,
                root_run_id TEXT,
                started_at INTEGER,
                finished_at INTEGER,
                created_at INTEGER DEFAULT (unixepoch()),
                updated_at INTEGER DEFAULT (unixepoch())
            )
            "#,
        )
        .execute(&pool)
        .await
        .context("Failed to create tasks table")?;

        self_migrate_task_columns(&pool).await?;

        Ok(Self { pool })
    }

    pub async fn create_task(
        &self,
        task_id: &str,
        specialist_id: &str,
        prompt: &str,
    ) -> Result<TaskRecord> {
        sqlx::query(
            "INSERT INTO tasks (task_id, specialist_id, prompt, status, attempt) VALUES (?, ?, ?, 'ready', 0)",
        )
        .bind(task_id)
        .bind(specialist_id)
        .bind(prompt)
        .execute(&self.pool)
        .await
        .context("Failed to insert task")?;

        Ok(TaskRecord {
            task_id: task_id.to_string(),
            specialist_id: specialist_id.to_string(),
            prompt: prompt.to_string(),
            status: TaskStatus::Ready,
            execution_status: TaskExecutionStatus::Pending,
            result_status: TaskResultStatus::Pending,
            attempt: 0,
            block_reason: None,
            result_json: None,
            finish_reason: None,
            error: None,
            parent_session_id: None,
            child_session_id: None,
            root_run_id: None,
            started_at: None,
            finished_at: None,
        })
    }

    pub async fn get_task(&self, task_id: &str) -> Result<Option<TaskRecord>> {
        let row = sqlx::query(
            "SELECT task_id, specialist_id, prompt, status, execution_status, result_status, attempt, block_reason, result_json, finish_reason, error, parent_session_id, child_session_id, root_run_id, started_at, finished_at FROM tasks WHERE task_id = ?",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to fetch task")?;

        match row {
            None => Ok(None),
            Some(row) => Ok(Some(task_record_from_row(row)?)),
        }
    }

    pub async fn list_child_tasks(&self, parent_session_id: &str) -> Result<Vec<TaskRecord>> {
        let rows = sqlx::query(
            "SELECT task_id, specialist_id, prompt, status, execution_status, result_status, attempt, block_reason, result_json, finish_reason, error, parent_session_id, child_session_id, root_run_id, started_at, finished_at FROM tasks WHERE parent_session_id = ? ORDER BY COALESCE(started_at, 0), task_id",
        )
        .bind(parent_session_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list child tasks")?;

        rows.into_iter().map(task_record_from_row).collect()
    }

    pub async fn update_status(
        &self,
        task_id: &str,
        status: &TaskStatus,
        block_reason: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE tasks SET status = ?, block_reason = ?, updated_at = unixepoch() WHERE task_id = ?",
        )
        .bind(status.as_str())
        .bind(block_reason)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .context("Failed to update task status")?;
        Ok(())
    }

    pub async fn increment_attempt(&self, task_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE tasks SET attempt = attempt + 1, updated_at = unixepoch() WHERE task_id = ?",
        )
        .bind(task_id)
        .execute(&self.pool)
        .await
        .context("Failed to increment attempt")?;
        Ok(())
    }

    pub async fn set_result(&self, task_id: &str, result_json: &str) -> Result<()> {
        sqlx::query("UPDATE tasks SET result_json = ?, updated_at = unixepoch() WHERE task_id = ?")
            .bind(result_json)
            .bind(task_id)
            .execute(&self.pool)
            .await
            .context("Failed to set task result")?;
        Ok(())
    }

    pub async fn set_result_json(&self, task_id: &str, result_json: &Value) -> Result<()> {
        let encoded =
            serde_json::to_string(result_json).context("Failed to encode task result JSON")?;
        self.set_result(task_id, &encoded).await
    }

    pub async fn mark_started(
        &self,
        task_id: &str,
        parent_session_id: Option<&str>,
        child_session_id: Option<&str>,
        root_run_id: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE tasks
            SET execution_status = 'running',
                result_status = 'pending',
                parent_session_id = COALESCE(?, parent_session_id),
                child_session_id = COALESCE(?, child_session_id),
                root_run_id = COALESCE(?, root_run_id),
                started_at = COALESCE(started_at, unixepoch()),
                finished_at = NULL,
                finish_reason = NULL,
                error = NULL,
                updated_at = unixepoch()
            WHERE task_id = ?
            "#,
        )
        .bind(parent_session_id)
        .bind(child_session_id)
        .bind(root_run_id)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .context("Failed to mark task started")?;
        Ok(())
    }

    pub async fn mark_finished(
        &self,
        task_id: &str,
        execution_status: &TaskExecutionStatus,
        result_status: &TaskResultStatus,
        finish_reason: Option<&str>,
        error: Option<&str>,
        root_run_id: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE tasks
            SET execution_status = ?,
                result_status = ?,
                finish_reason = ?,
                error = ?,
                root_run_id = COALESCE(?, root_run_id),
                finished_at = unixepoch(),
                updated_at = unixepoch()
            WHERE task_id = ?
            "#,
        )
        .bind(execution_status.as_str())
        .bind(result_status.as_str())
        .bind(finish_reason)
        .bind(error)
        .bind(root_run_id)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .context("Failed to mark task finished")?;
        Ok(())
    }
}

fn task_record_from_row(row: sqlx::sqlite::SqliteRow) -> Result<TaskRecord> {
    let status_str: String = row.try_get("status")?;
    let status = TaskStatus::from_str(&status_str).unwrap_or(TaskStatus::Failed);
    let execution_status_str: String = row.try_get("execution_status")?;
    let execution_status = TaskExecutionStatus::from_str(&execution_status_str)
        .unwrap_or(TaskExecutionStatus::Interrupted);
    let result_status_str: String = row.try_get("result_status")?;
    let result_status =
        TaskResultStatus::from_str(&result_status_str).unwrap_or(TaskResultStatus::NeedsReview);

    Ok(TaskRecord {
        task_id: row.try_get("task_id")?,
        specialist_id: row.try_get("specialist_id")?,
        prompt: row.try_get("prompt")?,
        status,
        execution_status,
        result_status,
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

async fn self_migrate_task_columns(pool: &SqlitePool) -> Result<()> {
    let columns = [
        ("execution_status", "TEXT NOT NULL DEFAULT 'pending'"),
        ("result_status", "TEXT NOT NULL DEFAULT 'pending'"),
        ("finish_reason", "TEXT"),
        ("error", "TEXT"),
        ("parent_session_id", "TEXT"),
        ("child_session_id", "TEXT"),
        ("root_run_id", "TEXT"),
        ("started_at", "INTEGER"),
        ("finished_at", "INTEGER"),
    ];

    for (name, definition) in columns {
        let sql = format!("ALTER TABLE tasks ADD COLUMN {name} {definition}");
        if let Err(err) = sqlx::query(&sql).execute(pool).await {
            let message = err.to_string();
            if !message.contains("duplicate column name") {
                return Err(err).context("Failed to migrate tasks table");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    #[tokio::test]
    async fn test_task_lifecycle() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        let store = TaskStore::new(pool).await?;

        // Create
        let rec = store.create_task("t1", "explore", "find files").await?;
        assert_eq!(rec.status, TaskStatus::Ready);
        assert_eq!(rec.attempt, 0);

        // Update status to Running
        store
            .update_status("t1", &TaskStatus::Running, None)
            .await?;
        store.increment_attempt("t1").await?;
        let rec = store.get_task("t1").await?.unwrap();
        assert_eq!(rec.status, TaskStatus::Running);
        assert_eq!(rec.attempt, 1);

        // Block
        store
            .update_status("t1", &TaskStatus::Blocked, Some("missing cap"))
            .await?;
        let rec = store.get_task("t1").await?.unwrap();
        assert_eq!(rec.status, TaskStatus::Blocked);
        assert_eq!(rec.block_reason.as_deref(), Some("missing cap"));

        // Resume + succeed
        store
            .update_status("t1", &TaskStatus::Running, None)
            .await?;
        store.increment_attempt("t1").await?;
        store
            .set_result_json("t1", &serde_json::json!({"output":"done"}))
            .await?;
        store
            .update_status("t1", &TaskStatus::Succeeded, None)
            .await?;
        let rec = store.get_task("t1").await?.unwrap();
        assert_eq!(rec.status, TaskStatus::Succeeded);
        assert_eq!(rec.attempt, 2);
        assert_eq!(rec.result_json.as_deref(), Some(r#"{"output":"done"}"#));

        Ok(())
    }
}
