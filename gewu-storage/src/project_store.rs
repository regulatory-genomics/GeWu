use anyhow::{Context, Result};
use sqlx::Row;
use sqlx::sqlite::SqlitePool;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProjectRecord {
    pub project_id: String,
    pub name: String,
    pub workdir: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct ProjectStore {
    pool: SqlitePool,
}

impl ProjectStore {
    pub async fn new(pool: SqlitePool) -> Result<Self> {
        // Schema in `gewu-storage/migrations/0007_projects.sql`.
        Ok(Self { pool })
    }

    pub async fn create(
        &self,
        name: &str,
        workdir: &str,
        created_at: i64,
    ) -> Result<ProjectRecord> {
        let project_id = format!("project_{}", Uuid::new_v4());
        sqlx::query(
            r#"INSERT INTO projects (project_id, name, workdir, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?)"#,
        )
        .bind(&project_id)
        .bind(name)
        .bind(workdir)
        .bind(created_at)
        .bind(created_at)
        .execute(&self.pool)
        .await
        .context("Failed to insert project")?;

        self.get(&project_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("project missing after insert"))
    }

    pub async fn create_or_get_by_workdir(
        &self,
        name: &str,
        workdir: &str,
        created_at: i64,
    ) -> Result<ProjectRecord> {
        let project_id = format!("project_{}", Uuid::new_v4());
        sqlx::query(
            r#"INSERT INTO projects (project_id, name, workdir, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?)
               ON CONFLICT(workdir) DO UPDATE SET updated_at = unixepoch()"#,
        )
        .bind(&project_id)
        .bind(name)
        .bind(workdir)
        .bind(created_at)
        .bind(created_at)
        .execute(&self.pool)
        .await
        .context("Failed to create or get project by workdir")?;

        self.get_by_workdir(workdir)
            .await?
            .ok_or_else(|| anyhow::anyhow!("project missing after workdir upsert"))
    }

    pub async fn get(&self, project_id: &str) -> Result<Option<ProjectRecord>> {
        let row = sqlx::query(
            "SELECT project_id, name, workdir, created_at, updated_at FROM projects WHERE project_id = ?",
        )
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get project")?;

        Ok(row.map(row_to_project))
    }

    pub async fn get_by_workdir(&self, workdir: &str) -> Result<Option<ProjectRecord>> {
        let row = sqlx::query(
            "SELECT project_id, name, workdir, created_at, updated_at FROM projects WHERE workdir = ?",
        )
        .bind(workdir)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get project by workdir")?;

        Ok(row.map(row_to_project))
    }

    pub async fn list_all(&self) -> Result<Vec<ProjectRecord>> {
        let rows = sqlx::query(
            "SELECT project_id, name, workdir, created_at, updated_at FROM projects ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to list projects")?;

        Ok(rows.into_iter().map(row_to_project).collect())
    }

    pub async fn update(&self, project_id: &str, name: &str, workdir: &str) -> Result<()> {
        sqlx::query(
            "UPDATE projects SET name = ?, workdir = ?, updated_at = unixepoch() WHERE project_id = ?",
        )
        .bind(name)
        .bind(workdir)
        .bind(project_id)
        .execute(&self.pool)
        .await
        .context("Failed to update project")?;
        Ok(())
    }

    pub async fn delete(&self, project_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM projects WHERE project_id = ?")
            .bind(project_id)
            .execute(&self.pool)
            .await
            .context("Failed to delete project")?;
        Ok(())
    }
}

fn row_to_project(row: sqlx::sqlite::SqliteRow) -> ProjectRecord {
    ProjectRecord {
        project_id: row.try_get("project_id").unwrap_or_default(),
        name: row.try_get("name").unwrap_or_default(),
        workdir: row.try_get("workdir").unwrap_or_default(),
        created_at: row.try_get("created_at").unwrap_or(0),
        updated_at: row.try_get("updated_at").unwrap_or(0),
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
    async fn test_project_crud() -> Result<()> {
        let store = ProjectStore::new(test_pool().await).await?;
        let project = store.create("GeWu", "/tmp/gewu", 1000).await?;

        assert_eq!(project.name, "GeWu");
        assert_eq!(project.workdir, "/tmp/gewu");

        let got = store.get(&project.project_id).await?.unwrap();
        assert_eq!(got, project);

        store
            .update(&project.project_id, "GeWu 2", "/tmp/gewu-2")
            .await?;
        let updated = store.get(&project.project_id).await?.unwrap();
        assert_eq!(updated.name, "GeWu 2");
        assert_eq!(updated.workdir, "/tmp/gewu-2");

        store.delete(&project.project_id).await?;
        assert!(store.get(&project.project_id).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn test_create_or_get_by_workdir_is_idempotent() -> Result<()> {
        let store = ProjectStore::new(test_pool().await).await?;
        let first = store
            .create_or_get_by_workdir("GeWu", "/tmp/gewu", 1000)
            .await?;
        let second = store
            .create_or_get_by_workdir("Different Name", "/tmp/gewu", 1001)
            .await?;

        assert_eq!(first.project_id, second.project_id);
        assert_eq!(second.name, "GeWu");
        assert_eq!(store.list_all().await?.len(), 1);
        Ok(())
    }
}
