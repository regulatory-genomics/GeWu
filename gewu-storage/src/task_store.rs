use anyhow::{Context, Result};
use gewu_core::types::{TaskRecord, TaskStatus};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

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
                attempt INTEGER NOT NULL DEFAULT 0,
                block_reason TEXT,
                result_json TEXT,
                created_at INTEGER DEFAULT (unixepoch()),
                updated_at INTEGER DEFAULT (unixepoch())
            )
            "#,
        )
        .execute(&pool)
        .await
        .context("Failed to create tasks table")?;

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
            attempt: 0,
            block_reason: None,
            result_json: None,
        })
    }

    pub async fn get_task(&self, task_id: &str) -> Result<Option<TaskRecord>> {
        let row = sqlx::query(
            "SELECT task_id, specialist_id, prompt, status, attempt, block_reason, result_json FROM tasks WHERE task_id = ?",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to fetch task")?;

        match row {
            None => Ok(None),
            Some(row) => {
                let status_str: String = row.try_get("status")?;
                let status = TaskStatus::from_str(&status_str)
                    .unwrap_or(TaskStatus::Failed);
                Ok(Some(TaskRecord {
                    task_id: row.try_get("task_id")?,
                    specialist_id: row.try_get("specialist_id")?,
                    prompt: row.try_get("prompt")?,
                    status,
                    attempt: row.try_get::<i32, _>("attempt")? as u32,
                    block_reason: row.try_get("block_reason")?,
                    result_json: row.try_get("result_json")?,
                }))
            }
        }
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
        sqlx::query(
            "UPDATE tasks SET result_json = ?, updated_at = unixepoch() WHERE task_id = ?",
        )
        .bind(result_json)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .context("Failed to set task result")?;
        Ok(())
    }
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
        store.update_status("t1", &TaskStatus::Running, None).await?;
        store.increment_attempt("t1").await?;
        let rec = store.get_task("t1").await?.unwrap();
        assert_eq!(rec.status, TaskStatus::Running);
        assert_eq!(rec.attempt, 1);

        // Block
        store.update_status("t1", &TaskStatus::Blocked, Some("missing cap")).await?;
        let rec = store.get_task("t1").await?.unwrap();
        assert_eq!(rec.status, TaskStatus::Blocked);
        assert_eq!(rec.block_reason.as_deref(), Some("missing cap"));

        // Resume + succeed
        store.update_status("t1", &TaskStatus::Running, None).await?;
        store.increment_attempt("t1").await?;
        store.set_result("t1", r#"{"output":"done"}"#).await?;
        store.update_status("t1", &TaskStatus::Succeeded, None).await?;
        let rec = store.get_task("t1").await?.unwrap();
        assert_eq!(rec.status, TaskStatus::Succeeded);
        assert_eq!(rec.attempt, 2);
        assert_eq!(rec.result_json.as_deref(), Some(r#"{"output":"done"}"#));

        Ok(())
    }
}