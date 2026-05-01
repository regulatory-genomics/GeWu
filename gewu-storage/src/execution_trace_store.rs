use anyhow::{Context, Result};
use gewu_core::types::{ExecutionEvent, ExecutionRun, ExecutionRunKind, ExecutionRunStatus};
use serde_json::Value;
use sqlx::Row;
use sqlx::sqlite::SqlitePool;

#[derive(Clone)]
pub struct ExecutionTraceStore {
    pool: SqlitePool,
}

#[derive(Clone, Debug)]
pub struct StartExecutionRun {
    pub parent_id: Option<String>,
    pub session_id: Option<String>,
    pub kind: ExecutionRunKind,
    pub name: String,
    pub input_json: Option<Value>,
}

impl ExecutionTraceStore {
    pub async fn new(pool: SqlitePool) -> Result<Self> {
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS execution_runs (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                session_id TEXT,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                status TEXT NOT NULL,
                input_json TEXT,
                output_json TEXT,
                error TEXT,
                started_at INTEGER NOT NULL,
                finished_at INTEGER
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create execution_runs table")?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS execution_events (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                FOREIGN KEY(run_id) REFERENCES execution_runs(id)
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create execution_events table")?;

        for statement in [
            "CREATE INDEX IF NOT EXISTS idx_execution_runs_session ON execution_runs(session_id)",
            "CREATE INDEX IF NOT EXISTS idx_execution_runs_parent ON execution_runs(parent_id)",
            "CREATE INDEX IF NOT EXISTS idx_execution_runs_status ON execution_runs(status)",
            "CREATE INDEX IF NOT EXISTS idx_execution_runs_started ON execution_runs(started_at)",
            "CREATE INDEX IF NOT EXISTS idx_execution_events_run ON execution_events(run_id)",
            "CREATE INDEX IF NOT EXISTS idx_execution_events_type ON execution_events(event_type)",
            "CREATE INDEX IF NOT EXISTS idx_execution_events_created ON execution_events(created_at)",
        ] {
            sqlx::query(statement)
                .execute(&self.pool)
                .await
                .with_context(|| format!("Failed to execute migration statement: {statement}"))?;
        }

        Ok(())
    }

    pub async fn start_run(&self, input: StartExecutionRun) -> Result<ExecutionRun> {
        let id = format!("run_{}", uuid::Uuid::new_v4());
        let now = now_unix();
        let input_json = encode_optional_json(input.input_json.as_ref())?;

        sqlx::query(
            r#"
            INSERT INTO execution_runs
            (id, parent_id, session_id, kind, name, status, input_json, started_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&id)
        .bind(&input.parent_id)
        .bind(&input.session_id)
        .bind(input.kind.as_str())
        .bind(&input.name)
        .bind(ExecutionRunStatus::Running.as_str())
        .bind(input_json)
        .bind(now)
        .execute(&self.pool)
        .await
        .context("Failed to insert execution run")?;

        self.get_run(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("created execution run not found"))
    }

    pub async fn finish_run(&self, id: &str, output_json: Option<&Value>) -> Result<ExecutionRun> {
        self.complete_run(id, ExecutionRunStatus::Succeeded, output_json, None)
            .await
    }

    pub async fn fail_run(&self, id: &str, error: &str) -> Result<ExecutionRun> {
        self.complete_run(id, ExecutionRunStatus::Failed, None, Some(error))
            .await
    }

    pub async fn cancel_run(&self, id: &str, error: Option<&str>) -> Result<ExecutionRun> {
        self.complete_run(id, ExecutionRunStatus::Cancelled, None, error)
            .await
    }

    pub async fn skip_run(&self, id: &str, error: Option<&str>) -> Result<ExecutionRun> {
        self.complete_run(id, ExecutionRunStatus::Skipped, None, error)
            .await
    }

    async fn complete_run(
        &self,
        id: &str,
        status: ExecutionRunStatus,
        output_json: Option<&Value>,
        error: Option<&str>,
    ) -> Result<ExecutionRun> {
        let now = now_unix();
        let output_json = encode_optional_json(output_json)?;
        sqlx::query(
            r#"
            UPDATE execution_runs
            SET status = ?, output_json = ?, error = ?, finished_at = ?
            WHERE id = ?
            "#,
        )
        .bind(status.as_str())
        .bind(output_json)
        .bind(error)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await
        .context("Failed to update execution run")?;

        self.get_run(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("completed execution run not found"))
    }

    pub async fn record_event(
        &self,
        run_id: &str,
        event_type: &str,
        payload_json: &Value,
    ) -> Result<ExecutionEvent> {
        let id = format!("evt_{}", uuid::Uuid::new_v4());
        let now = now_unix();
        let payload_json = serde_json::to_string(payload_json)
            .context("Failed to encode execution event payload")?;

        sqlx::query(
            r#"
            INSERT INTO execution_events
            (id, run_id, event_type, payload_json, created_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(&id)
        .bind(run_id)
        .bind(event_type)
        .bind(payload_json)
        .bind(now)
        .execute(&self.pool)
        .await
        .context("Failed to insert execution event")?;

        self.get_event(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("created execution event not found"))
    }

    pub async fn get_run(&self, id: &str) -> Result<Option<ExecutionRun>> {
        let row = sqlx::query("SELECT * FROM execution_runs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .context("Failed to fetch execution run")?;
        row.map(row_to_run).transpose()
    }

    pub async fn root_runs_for_session(&self, session_id: &str) -> Result<Vec<ExecutionRun>> {
        let rows = sqlx::query(
            r#"
            SELECT * FROM execution_runs
            WHERE session_id = ? AND parent_id IS NULL
            ORDER BY started_at ASC
            "#,
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list root execution runs for session")?;

        rows.into_iter().map(row_to_run).collect()
    }

    pub async fn runs_for_session(&self, session_id: &str) -> Result<Vec<ExecutionRun>> {
        let rows = sqlx::query(
            r#"
            SELECT * FROM execution_runs
            WHERE session_id = ?
            ORDER BY started_at ASC
            "#,
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list execution runs for session")?;

        rows.into_iter().map(row_to_run).collect()
    }

    pub async fn child_runs(&self, parent_id: &str) -> Result<Vec<ExecutionRun>> {
        let rows = sqlx::query(
            r#"
            SELECT * FROM execution_runs
            WHERE parent_id = ?
            ORDER BY started_at ASC
            "#,
        )
        .bind(parent_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list child execution runs")?;

        rows.into_iter().map(row_to_run).collect()
    }

    pub async fn events_for_run(&self, run_id: &str) -> Result<Vec<ExecutionEvent>> {
        let rows = sqlx::query(
            r#"
            SELECT * FROM execution_events
            WHERE run_id = ?
            ORDER BY created_at ASC
            "#,
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list execution events")?;

        rows.into_iter().map(row_to_event).collect()
    }

    async fn get_event(&self, id: &str) -> Result<Option<ExecutionEvent>> {
        let row = sqlx::query("SELECT * FROM execution_events WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .context("Failed to fetch execution event")?;
        row.map(row_to_event).transpose()
    }
}

fn row_to_run(row: sqlx::sqlite::SqliteRow) -> Result<ExecutionRun> {
    let kind: String = row.try_get("kind")?;
    let status: String = row.try_get("status")?;
    Ok(ExecutionRun {
        id: row.try_get("id")?,
        parent_id: row.try_get("parent_id")?,
        session_id: row.try_get("session_id")?,
        kind: ExecutionRunKind::from_str(&kind)
            .ok_or_else(|| anyhow::anyhow!("unknown execution run kind '{kind}'"))?,
        name: row.try_get("name")?,
        status: ExecutionRunStatus::from_str(&status)
            .ok_or_else(|| anyhow::anyhow!("unknown execution run status '{status}'"))?,
        input_json: decode_optional_json(row.try_get("input_json")?)?,
        output_json: decode_optional_json(row.try_get("output_json")?)?,
        error: row.try_get("error")?,
        started_at: row.try_get("started_at")?,
        finished_at: row.try_get("finished_at")?,
    })
}

fn row_to_event(row: sqlx::sqlite::SqliteRow) -> Result<ExecutionEvent> {
    let payload_json: String = row.try_get("payload_json")?;
    Ok(ExecutionEvent {
        id: row.try_get("id")?,
        run_id: row.try_get("run_id")?,
        event_type: row.try_get("event_type")?,
        payload_json: serde_json::from_str(&payload_json)
            .context("Failed to decode execution event payload")?,
        created_at: row.try_get("created_at")?,
    })
}

fn encode_optional_json(value: Option<&Value>) -> Result<Option<String>> {
    value
        .map(|value| serde_json::to_string(value).context("Failed to encode execution JSON"))
        .transpose()
}

fn decode_optional_json(value: Option<String>) -> Result<Option<Value>> {
    value
        .map(|value| serde_json::from_str(&value).context("Failed to decode execution JSON"))
        .transpose()
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    #[tokio::test]
    async fn execution_run_tree_lifecycle() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        let store = ExecutionTraceStore::new(pool).await?;

        let workflow = store
            .start_run(StartExecutionRun {
                parent_id: None,
                session_id: Some("session-1".to_string()),
                kind: ExecutionRunKind::Workflow,
                name: "daily_opencode_rollup".to_string(),
                input_json: Some(serde_json::json!({"date":"2026-04-29"})),
            })
            .await?;

        let node = store
            .start_run(StartExecutionRun {
                parent_id: Some(workflow.id.clone()),
                session_id: Some("session-1".to_string()),
                kind: ExecutionRunKind::WorkflowNode,
                name: "memory_sql_query".to_string(),
                input_json: Some(serde_json::json!({"limit":10})),
            })
            .await?;

        store
            .record_event(&node.id, "tool_start", &serde_json::json!({"tool":"memory_sql_query"}))
            .await?;
        store
            .finish_run(&node.id, Some(&serde_json::json!({"row_count": 10})))
            .await?;
        store
            .finish_run(&workflow.id, Some(&serde_json::json!({"status":"ok"})))
            .await?;

        let children = store.child_runs(&workflow.id).await?;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, node.id);

        let events = store.events_for_run(&node.id).await?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "tool_start");

        let root_runs = store.root_runs_for_session("session-1").await?;
        assert_eq!(root_runs.len(), 1);
        assert_eq!(root_runs[0].status, ExecutionRunStatus::Succeeded);

        Ok(())
    }
}
