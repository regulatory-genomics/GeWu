use anyhow::Result;
use sqlx::{Row, SqlitePool};

use crate::scheduler::job::{CreateJobRequest, JobRun, RunEvent, ScheduledJob, UpdateJobRequest};

#[derive(Clone)]
pub struct JobStore {
    pool: SqlitePool,
}

impl JobStore {
    pub async fn new(pool: SqlitePool) -> Result<Self> {
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS scheduled_jobs (
              id TEXT PRIMARY KEY,
              name TEXT NOT NULL,
              enabled INTEGER NOT NULL,
              schedule TEXT NOT NULL,
              timezone TEXT NOT NULL,
              specialist_id TEXT NOT NULL,
              prompt TEXT NOT NULL,
              workdir TEXT,
              no_overlap INTEGER NOT NULL,
              timeout_seconds INTEGER,
              next_run_at INTEGER,
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS job_runs (
              id TEXT PRIMARY KEY,
              job_id TEXT NOT NULL,
              status TEXT NOT NULL,
              started_at INTEGER,
              finished_at INTEGER,
              duration_ms INTEGER,
              prompt TEXT,
              output TEXT,
              error TEXT,
              log_path TEXT,
              created_at INTEGER NOT NULL,
              FOREIGN KEY(job_id) REFERENCES scheduled_jobs(id)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS run_events (
              id TEXT PRIMARY KEY,
              run_id TEXT NOT NULL,
              event_type TEXT NOT NULL,
              timestamp INTEGER NOT NULL,
              payload TEXT,
              FOREIGN KEY(run_id) REFERENCES job_runs(id)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_run_events_run_id ON run_events(run_id)")
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    pub async fn create_job(&self, request: CreateJobRequest) -> Result<ScheduledJob> {
        let now = now_unix();
        let next_run_at = Some(next_run_after(&request.schedule, now));
        sqlx::query(
            r#"
            INSERT INTO scheduled_jobs
            (id, name, enabled, schedule, timezone, specialist_id, prompt, workdir, no_overlap, timeout_seconds, next_run_at, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&request.id)
        .bind(&request.name)
        .bind(request.enabled)
        .bind(&request.schedule)
        .bind(&request.timezone)
        .bind(&request.specialist_id)
        .bind(&request.prompt)
        .bind(&request.workdir)
        .bind(request.no_overlap)
        .bind(request.timeout_seconds)
        .bind(next_run_at)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.get_job(&request.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("created job not found"))
    }

    pub async fn list_jobs(&self) -> Result<Vec<ScheduledJob>> {
        let rows = sqlx::query("SELECT * FROM scheduled_jobs ORDER BY id ASC")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_to_job).collect()
    }

    pub async fn due_jobs(&self, now: i64) -> Result<Vec<ScheduledJob>> {
        let rows = sqlx::query(
            "SELECT * FROM scheduled_jobs WHERE enabled = 1 AND next_run_at IS NOT NULL AND next_run_at <= ? ORDER BY next_run_at ASC",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_job).collect()
    }

    pub async fn get_job(&self, id: &str) -> Result<Option<ScheduledJob>> {
        let row = sqlx::query("SELECT * FROM scheduled_jobs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_job).transpose()
    }

    pub async fn update_job(
        &self,
        id: &str,
        request: UpdateJobRequest,
    ) -> Result<Option<ScheduledJob>> {
        let Some(mut job) = self.get_job(id).await? else {
            return Ok(None);
        };
        if let Some(name) = request.name {
            job.name = name;
        }
        if let Some(enabled) = request.enabled {
            job.enabled = enabled;
        }
        if let Some(schedule) = request.schedule {
            job.schedule = schedule;
            job.next_run_at = Some(next_run_after(&job.schedule, now_unix()));
        }
        if let Some(timezone) = request.timezone {
            job.timezone = timezone;
        }
        if let Some(specialist_id) = request.specialist_id {
            job.specialist_id = specialist_id;
        }
        if let Some(prompt) = request.prompt {
            job.prompt = prompt;
        }
        if let Some(workdir) = request.workdir {
            job.workdir = Some(workdir);
        }
        if let Some(no_overlap) = request.no_overlap {
            job.no_overlap = no_overlap;
        }
        if let Some(timeout_seconds) = request.timeout_seconds {
            job.timeout_seconds = Some(timeout_seconds);
        }
        if let Some(next_run_at) = request.next_run_at {
            job.next_run_at = Some(next_run_at);
        }
        job.updated_at = now_unix();
        sqlx::query(
            r#"
            UPDATE scheduled_jobs SET
              name = ?, enabled = ?, schedule = ?, timezone = ?, specialist_id = ?, prompt = ?, workdir = ?,
              no_overlap = ?, timeout_seconds = ?, next_run_at = ?, updated_at = ?
            WHERE id = ?
            "#,
        )
        .bind(&job.name)
        .bind(job.enabled)
        .bind(&job.schedule)
        .bind(&job.timezone)
        .bind(&job.specialist_id)
        .bind(&job.prompt)
        .bind(&job.workdir)
        .bind(job.no_overlap)
        .bind(job.timeout_seconds)
        .bind(job.next_run_at)
        .bind(job.updated_at)
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.get_job(id).await
    }

    pub async fn delete_job(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM scheduled_jobs WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn create_run(&self, job_id: &str, prompt: &str) -> Result<JobRun> {
        let now = now_unix();
        let id = format!("run_{}", uuid::Uuid::new_v4());
        sqlx::query(
            "INSERT INTO job_runs (id, job_id, status, started_at, prompt, created_at) VALUES (?, ?, 'running', ?, ?, ?)",
        )
        .bind(&id)
        .bind(job_id)
        .bind(now)
        .bind(prompt)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.get_run(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("created run not found"))
    }

    pub async fn has_running_run(&self, job_id: &str) -> Result<bool> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM job_runs WHERE job_id = ? AND status = 'running'",
        )
        .bind(job_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    pub async fn finish_run(
        &self,
        id: &str,
        output: Option<String>,
        error: Option<String>,
    ) -> Result<JobRun> {
        let now = now_unix();
        let status = if error.is_some() { "failed" } else { "success" };
        let started_at = self
            .get_run(id)
            .await?
            .and_then(|run| run.started_at)
            .unwrap_or(now);
        let duration_ms = (now - started_at) * 1000;
        sqlx::query("UPDATE job_runs SET status = ?, finished_at = ?, duration_ms = ?, output = ?, error = ? WHERE id = ?")
            .bind(status)
            .bind(now)
            .bind(duration_ms)
            .bind(output)
            .bind(error)
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.get_run(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("finished run not found"))
    }

    pub async fn list_runs(&self) -> Result<Vec<JobRun>> {
        let rows = sqlx::query("SELECT * FROM job_runs ORDER BY created_at DESC LIMIT 200")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_to_run).collect()
    }

    pub async fn get_run(&self, id: &str) -> Result<Option<JobRun>> {
        let row = sqlx::query("SELECT * FROM job_runs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_run).transpose()
    }

    pub async fn advance_job(&self, job: &ScheduledJob) -> Result<()> {
        let next_run_at = next_run_after(&job.schedule, now_unix());
        sqlx::query("UPDATE scheduled_jobs SET next_run_at = ?, updated_at = ? WHERE id = ?")
            .bind(next_run_at)
            .bind(now_unix())
            .bind(&job.id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn insert_run_event(&self, event: &RunEvent) -> Result<()> {
        sqlx::query(
            "INSERT INTO run_events (id, run_id, event_type, timestamp, payload) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&event.id)
        .bind(&event.run_id)
        .bind(&event.event_type)
        .bind(event.timestamp)
        .bind(&event.payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert_run_events(&self, events: &[RunEvent]) -> Result<()> {
        for event in events {
            self.insert_run_event(event).await?;
        }
        Ok(())
    }

    pub async fn list_run_events(&self, run_id: &str) -> Result<Vec<RunEvent>> {
        let rows = sqlx::query("SELECT * FROM run_events WHERE run_id = ? ORDER BY timestamp ASC")
            .bind(run_id)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(RunEvent {
                    id: row.try_get("id")?,
                    run_id: row.try_get("run_id")?,
                    event_type: row.try_get("event_type")?,
                    timestamp: row.try_get("timestamp")?,
                    payload: row.try_get("payload")?,
                })
            })
            .collect()
    }
}

fn row_to_job(row: sqlx::sqlite::SqliteRow) -> Result<ScheduledJob> {
    Ok(ScheduledJob {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        enabled: row.try_get::<bool, _>("enabled")?,
        schedule: row.try_get("schedule")?,
        timezone: row.try_get("timezone")?,
        specialist_id: row.try_get("specialist_id")?,
        prompt: row.try_get("prompt")?,
        workdir: row.try_get("workdir")?,
        no_overlap: row.try_get::<bool, _>("no_overlap")?,
        timeout_seconds: row.try_get("timeout_seconds")?,
        next_run_at: row.try_get("next_run_at")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn row_to_run(row: sqlx::sqlite::SqliteRow) -> Result<JobRun> {
    Ok(JobRun {
        id: row.try_get("id")?,
        job_id: row.try_get("job_id")?,
        status: row.try_get("status")?,
        started_at: row.try_get("started_at")?,
        finished_at: row.try_get("finished_at")?,
        duration_ms: row.try_get("duration_ms")?,
        prompt: row.try_get("prompt")?,
        output: row.try_get("output")?,
        error: row.try_get("error")?,
        log_path: row.try_get("log_path")?,
        created_at: row.try_get("created_at")?,
    })
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn next_run_after(schedule: &str, from: i64) -> i64 {
    use chrono::DateTime;
    use std::str::FromStr;

    if let Ok(parsed) = cron::Schedule::from_str(&format!("0 {schedule}")) {
        let dt = DateTime::from_timestamp(from, 0).unwrap_or_default();
        if let Some(next) = parsed.after(&dt).next() {
            return next.timestamp();
        }
    }
    from + 3600
}
