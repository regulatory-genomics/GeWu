use anyhow::{Context, Result};
use sqlx::{Row, sqlite::SqlitePool};
use tracing::info;

/// Metadata for a persisted session.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub project_id: Option<String>,
    pub workdir: Option<String>,
    pub agent_type: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Durable session store backed by SQLite.
///
/// This is the single source of truth for session existence. The in-memory
/// `agents` map only caches live runtime handles; it is *not* authoritative.
pub struct SessionStore {
    pool: SqlitePool,
}

impl SessionStore {
    pub async fn new(pool: SqlitePool) -> Result<Self> {
        // Schema lives in `gewu-storage/migrations/0002_sessions.sql`.
        // The runner is invoked from each binary's startup
        // (gewu-web/state.rs, gewu-cli/lib.rs, gewu-gateway/lib.rs).
        // Backwards-compat shim: if the table existed pre-migrations without
        // project_id/workdir columns, ALTER them in. Errors are tolerated for
        // duplicate columns.
        for stmt in [
            "ALTER TABLE sessions ADD COLUMN project_id TEXT",
            "ALTER TABLE sessions ADD COLUMN workdir TEXT",
        ] {
            if let Err(err) = sqlx::query(stmt).execute(&pool).await {
                let message = err.to_string();
                if !message.contains("duplicate column name") {
                    return Err(err).context("Failed to backfill sessions schema");
                }
            }
        }
        Ok(Self { pool })
    }

    /// Backfill sessions from `memory_messages` that have no row in `sessions`.
    /// Called once at startup so historical sessions become visible.
    pub async fn backfill_from_history(&self) -> Result<u64> {
        let result = sqlx::query(
            r#"
            INSERT OR IGNORE INTO sessions (session_id, agent_type, created_at, updated_at)
            SELECT
                m.session_id,
                'unknown',
                MIN(m.created_at),
                MAX(m.created_at)
            FROM memory_messages m
            LEFT JOIN sessions s ON s.session_id = m.session_id
            WHERE s.session_id IS NULL
            GROUP BY m.session_id
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to backfill sessions from history")?;

        let count = result.rows_affected();
        if count > 0 {
            info!(
                "backfilled {} historical session(s) into sessions table",
                count
            );
        }
        Ok(count)
    }

    pub async fn backfill_workdir(&self, default_workdir: &str) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE sessions
            SET workdir = ?, updated_at = unixepoch()
            WHERE workdir IS NULL OR workdir = ''
            "#,
        )
        .bind(default_workdir)
        .execute(&self.pool)
        .await
        .context("Failed to backfill session workdir")?;

        let count = result.rows_affected();
        if count > 0 {
            info!("backfilled workdir for {} historical session(s)", count);
        }
        Ok(count)
    }

    pub async fn insert(
        &self,
        session_id: &str,
        project_id: Option<&str>,
        workdir: Option<&str>,
        agent_type: &str,
        created_at: i64,
    ) -> Result<SessionRecord> {
        sqlx::query(
            "INSERT INTO sessions (session_id, project_id, workdir, agent_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(project_id)
        .bind(workdir)
        .bind(agent_type)
        .bind(created_at)
        .bind(created_at)
        .execute(&self.pool)
        .await
        .context("Failed to insert session")?;

        Ok(SessionRecord {
            session_id: session_id.to_string(),
            project_id: project_id.map(ToOwned::to_owned),
            workdir: workdir.map(ToOwned::to_owned),
            agent_type: agent_type.to_string(),
            created_at,
            updated_at: created_at,
        })
    }

    pub async fn get(&self, session_id: &str) -> Result<Option<SessionRecord>> {
        let row = sqlx::query(
            "SELECT session_id, project_id, workdir, agent_type, created_at, updated_at FROM sessions WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get session")?;

        Ok(match row {
            Some(r) => Some(SessionRecord {
                session_id: r.try_get("session_id").context("deserialize session_id")?,
                project_id: r.try_get("project_id").context("deserialize project_id")?,
                workdir: r.try_get("workdir").context("deserialize workdir")?,
                agent_type: r.try_get("agent_type").context("deserialize agent_type")?,
                created_at: r.try_get("created_at").context("deserialize created_at")?,
                updated_at: r.try_get("updated_at").context("deserialize updated_at")?,
            }),
            None => None,
        })
    }

    /// Resolve a short prefix to a full session_id. Returns `None` if zero
    /// or more than one match.
    pub async fn resolve_prefix(&self, prefix: &str) -> Result<Option<String>> {
        let pattern = format!("{prefix}%");
        let rows = sqlx::query("SELECT session_id FROM sessions WHERE session_id LIKE ? LIMIT 2")
            .bind(&pattern)
            .fetch_all(&self.pool)
            .await
            .context("Failed to resolve session prefix")?;

        if rows.len() == 1 {
            Ok(Some(rows[0].try_get("session_id")?))
        } else {
            Ok(None)
        }
    }

    pub async fn list_all(&self) -> Result<Vec<SessionRecord>> {
        let rows = sqlx::query(
            "SELECT session_id, project_id, workdir, agent_type, created_at, updated_at FROM sessions ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to list sessions")?;

        let mut records = Vec::with_capacity(rows.len());
        for r in rows {
            records.push(SessionRecord {
                session_id: r.try_get("session_id").context("deserialize session_id")?,
                project_id: r.try_get("project_id").context("deserialize project_id")?,
                workdir: r.try_get("workdir").context("deserialize workdir")?,
                agent_type: r.try_get("agent_type").context("deserialize agent_type")?,
                created_at: r.try_get("created_at").context("deserialize created_at")?,
                updated_at: r.try_get("updated_at").context("deserialize updated_at")?,
            });
        }
        Ok(records)
    }

    pub async fn list_by_project(&self, project_id: &str) -> Result<Vec<SessionRecord>> {
        let rows = sqlx::query(
            "SELECT session_id, project_id, workdir, agent_type, created_at, updated_at FROM sessions WHERE project_id = ? ORDER BY updated_at DESC",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list sessions by project")?;

        let mut records = Vec::with_capacity(rows.len());
        for r in rows {
            records.push(SessionRecord {
                session_id: r.try_get("session_id").context("deserialize session_id")?,
                project_id: r.try_get("project_id").context("deserialize project_id")?,
                workdir: r.try_get("workdir").context("deserialize workdir")?,
                agent_type: r.try_get("agent_type").context("deserialize agent_type")?,
                created_at: r.try_get("created_at").context("deserialize created_at")?,
                updated_at: r.try_get("updated_at").context("deserialize updated_at")?,
            });
        }
        Ok(records)
    }

    pub async fn update_project_id(
        &self,
        session_id: &str,
        project_id: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE sessions SET project_id = ?, updated_at = unixepoch() WHERE session_id = ?",
        )
        .bind(project_id)
        .bind(session_id)
        .execute(&self.pool)
        .await
        .context("Failed to update session project_id")?;
        Ok(())
    }

    pub async fn update_project_and_workdir(
        &self,
        session_id: &str,
        project_id: Option<&str>,
        workdir: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE sessions SET project_id = ?, workdir = ?, updated_at = unixepoch() WHERE session_id = ?",
        )
        .bind(project_id)
        .bind(workdir)
        .bind(session_id)
        .execute(&self.pool)
        .await
        .context("Failed to update session project_id/workdir")?;
        Ok(())
    }

    pub async fn update_agent_type(&self, session_id: &str, agent_type: &str) -> Result<()> {
        sqlx::query(
            "UPDATE sessions SET agent_type = ?, updated_at = unixepoch() WHERE session_id = ?",
        )
        .bind(agent_type)
        .bind(session_id)
        .execute(&self.pool)
        .await
        .context("Failed to update session agent_type")?;
        Ok(())
    }

    pub async fn touch(&self, session_id: &str) -> Result<()> {
        sqlx::query("UPDATE sessions SET updated_at = unixepoch() WHERE session_id = ?")
            .bind(session_id)
            .execute(&self.pool)
            .await
            .context("Failed to touch session")?;
        Ok(())
    }

    pub async fn delete(&self, session_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE session_id = ?")
            .bind(session_id)
            .execute(&self.pool)
            .await
            .context("Failed to delete session")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::run_migrations(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn test_crud() -> Result<()> {
        let pool = test_pool().await;
        let store = SessionStore::new(pool).await?;

        let rec = store
            .insert(
                "s1",
                Some("project-1"),
                Some("/tmp/project-1"),
                "general",
                1000,
            )
            .await?;
        assert_eq!(rec.session_id, "s1");
        assert_eq!(rec.project_id.as_deref(), Some("project-1"));
        assert_eq!(rec.workdir.as_deref(), Some("/tmp/project-1"));
        assert_eq!(rec.agent_type, "general");

        let got = store.get("s1").await?.unwrap();
        assert_eq!(got.project_id.as_deref(), Some("project-1"));
        assert_eq!(got.workdir.as_deref(), Some("/tmp/project-1"));
        assert_eq!(got.agent_type, "general");

        let sessions = store.list_by_project("project-1").await?;
        assert_eq!(sessions.len(), 1);

        store.update_project_id("s1", None).await?;
        let got = store.get("s1").await?.unwrap();
        assert_eq!(got.project_id, None);

        store.update_agent_type("s1", "coder").await?;
        let got = store.get("s1").await?.unwrap();
        assert_eq!(got.agent_type, "coder");

        store.delete("s1").await?;
        assert!(store.get("s1").await?.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_resolve_prefix() -> Result<()> {
        let pool = test_pool().await;
        let store = SessionStore::new(pool).await?;

        store.insert("abc-123", None, None, "general", 1000).await?;
        store.insert("abc-456", None, None, "general", 1001).await?;
        store.insert("def-789", None, None, "general", 1002).await?;

        // Unique prefix
        assert_eq!(store.resolve_prefix("def").await?, Some("def-789".into()));
        // Ambiguous prefix
        assert_eq!(store.resolve_prefix("abc").await?, None);
        // No match
        assert_eq!(store.resolve_prefix("zzz").await?, None);

        Ok(())
    }

    #[tokio::test]
    async fn test_backfill() -> Result<()> {
        let pool = test_pool().await;
        let store = SessionStore::new(pool.clone()).await?;

        // Insert some history messages
        sqlx::query(
            "INSERT INTO memory_messages (session_id, content, created_at) VALUES (?, ?, ?)",
        )
        .bind("old-session-1")
        .bind("{}")
        .bind(500)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO memory_messages (session_id, content, created_at) VALUES (?, ?, ?)",
        )
        .bind("old-session-1")
        .bind("{}")
        .bind(600)
        .execute(&pool)
        .await?;

        let count = store.backfill_from_history().await?;
        assert_eq!(count, 1);

        let got = store.get("old-session-1").await?.unwrap();
        assert_eq!(got.project_id, None);
        assert_eq!(got.workdir, None);
        assert_eq!(got.agent_type, "unknown");
        assert_eq!(got.created_at, 500);
        assert_eq!(got.updated_at, 600);

        let workdir_count = store.backfill_workdir("/tmp/gewu").await?;
        assert_eq!(workdir_count, 1);
        let got = store.get("old-session-1").await?.unwrap();
        assert_eq!(got.workdir.as_deref(), Some("/tmp/gewu"));

        // Second backfill should be idempotent
        let count2 = store.backfill_from_history().await?;
        assert_eq!(count2, 0);

        Ok(())
    }
}
