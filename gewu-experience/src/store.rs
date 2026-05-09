//! Experience store implementation combining SQLite and Qdrant.

use crate::config::Config;
use crate::types::{Experience, ExperienceQuery, RecalledExperience, StoreResult};
use anyhow::{Context, Result};
use embed_anything::embeddings::embed::{Embedder, EmbeddingResult};
use qdrant_client::qdrant::{
    Distance, NamedVectors, PointStruct, Query, QueryPointsBuilder, VectorParamsBuilder, VectorsConfig,
};
use qdrant_client::Qdrant;
use sqlx::sqlite::SqliteRow;
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

pub struct ExperienceStore {
    pool: sqlx::SqlitePool,
    qdrant: Arc<Qdrant>,
    embedding_model: RwLock<Embedder>,
    config: Config,
}

impl ExperienceStore {
    pub async fn new(config: Config) -> Result<Self> {
        let db_url = if config.db_path.is_absolute() {
            format!("sqlite://{}?mode=rwc", config.db_path.display())
        } else {
            format!("sqlite:{}?mode=rwc", config.db_path.display())
        };

        if let Some(parent) = config.db_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).context("Failed to create database directory")?;
            }
        }

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&db_url)
            .await
            .context("Failed to connect to SQLite")?;

        let store = Self {
            pool,
            qdrant: Self::create_qdrant_client(&config)?,
            embedding_model: RwLock::new(Embedder::from_pretrained_hf(
                &config.embedding_model,
                None,
                None,
                None,
            )?),
            config,
        };

        store.init_schema().await?;
        store.init_qdrant_collection().await?;

        Ok(store)
    }

    fn create_qdrant_client(config: &Config) -> Result<Arc<Qdrant>> {
        let mut builder = Qdrant::from_url(&config.qdrant_url);
        if let Some(ref api_key) = config.qdrant_api_key {
            builder = builder.api_key(api_key.as_str());
        }
        let client = builder.build()?;
        Ok(Arc::new(client))
    }

    async fn init_schema(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS experiences (
                id TEXT PRIMARY KEY,
                task_description TEXT NOT NULL,
                task_type TEXT NOT NULL,
                problem_summary TEXT NOT NULL,
                solution_summary TEXT NOT NULL,
                tools_used TEXT NOT NULL,
                iteration_count INTEGER NOT NULL,
                success INTEGER NOT NULL,
                embedding_id TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                quality_score REAL DEFAULT 0.0,
                recall_count INTEGER DEFAULT 0
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create experiences table")?;

        let alter_result = sqlx::query(
            "ALTER TABLE experiences ADD COLUMN quality_score REAL DEFAULT 0.0"
        )
        .execute(&self.pool)
        .await;

        if let Err(e) = alter_result {
            tracing::debug!("Alter quality_score (may already exist): {}", e);
        }

        let alter_result2 = sqlx::query(
            "ALTER TABLE experiences ADD COLUMN recall_count INTEGER DEFAULT 0"
        )
        .execute(&self.pool)
        .await;

        if let Err(e) = alter_result2 {
            tracing::debug!("Alter recall_count (may already exist): {}", e);
        }

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_experiences_task_type ON experiences(task_type)"
        )
        .execute(&self.pool)
        .await
        .context("Failed to create task_type index")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_experiences_success ON experiences(success)"
        )
        .execute(&self.pool)
        .await
        .context("Failed to create success index")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_experiences_created_at ON experiences(created_at)"
        )
        .execute(&self.pool)
        .await
        .context("Failed to create created_at index")?;

        info!("Experience database schema initialized");
        Ok(())
    }

    async fn init_qdrant_collection(&self) -> Result<()> {
        let collection_name = &self.config.collection_name;

        let exists = self.qdrant.collection_exists(collection_name).await?;

        if !exists {
            let vectors_config = VectorsConfig {
                config: Some(
                    qdrant_client::qdrant::vectors_config::Config::from(
                        qdrant_client::qdrant::VectorParamsMap {
                            map: HashMap::from([(
                                "dense".to_string(),
                                VectorParamsBuilder::new(
                                    self.config.embedding_dimension as u64,
                                    Distance::Cosine,
                                )
                                .build(),
                            )]),
                        },
                    ),
                ),
            };

            let create = qdrant_client::qdrant::CreateCollectionBuilder::new(collection_name)
                .vectors_config(vectors_config);

            self.qdrant.create_collection(create).await?;
            info!("Created Qdrant collection: {}", collection_name);
        }

        Ok(())
    }

    pub async fn store(&self, experience: &Experience) -> Result<()> {
        let tools_json = serde_json::to_string(&experience.tools_used)
            .context("Failed to serialize tools_used")?;

        sqlx::query(
            r#"
            INSERT INTO experiences
            (id, task_description, task_type, problem_summary, solution_summary,
             tools_used, iteration_count, success, embedding_id, created_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&experience.id)
        .bind(&experience.task_description)
        .bind(&experience.task_type)
        .bind(&experience.problem_summary)
        .bind(&experience.solution_summary)
        .bind(&tools_json)
        .bind(experience.iteration_count)
        .bind(experience.success as i32)
        .bind(&experience.embedding_id)
        .bind(experience.created_at.timestamp())
        .execute(&self.pool)
        .await
        .context("Failed to insert experience into SQLite")?;

        let search_text = experience.search_text();
        let embedding = self.compute_embedding(&search_text).await?;

        let mut payload = qdrant_client::Payload::new();
        payload.insert(
            "experience_id".to_string(),
            qdrant_client::qdrant::Value::from(experience.id.clone()),
        );
        payload.insert(
            "task_type".to_string(),
            qdrant_client::qdrant::Value::from(experience.task_type.clone()),
        );

        let point_id = experience
            .embedding_id
            .parse::<u64>()
            .unwrap_or_else(|_| {
                experience
                    .id
                    .split('-')
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0)
            });

        let named_vectors = NamedVectors::default()
            .add_vector("dense", embedding);

        let point = PointStruct::new(point_id, named_vectors, payload);

        let collection_name = &self.config.collection_name;
        self.qdrant
            .upsert_points(
                qdrant_client::qdrant::UpsertPointsBuilder::new(collection_name, vec![point])
                    .wait(true),
            )
            .await
            .context("Failed to upsert embedding to Qdrant")?;

        info!(
            "Stored experience: {} ({})",
            experience.id, experience.task_type
        );
        Ok(())
    }

    pub async fn smart_store(
        &self,
        task_description: &str,
        task_type: &str,
        solution: &str,
        tools_used: &[String],
        success: bool,
    ) -> Result<StoreResult> {
        let quality_score = Experience::compute_quality_score(solution);

        if quality_score < 5.0 {
            return Ok(StoreResult::Skipped {
                reason: "Quality score below threshold (5.0)".to_string(),
            });
        }

        let query = ExperienceQuery {
            query_text: task_description.to_string(),
            task_type_filter: None,
            limit: 3,
            score_threshold: 0.7,
        };

        let similar = self.recall(&query).await?;

        if let Some(best_match) = similar.first() {
            let similarity = best_match.similarity_score;

            if similarity > 0.8 {
                if quality_score > best_match.experience.quality_score {
                    self.update_solution(&best_match.experience.id, solution, quality_score).await?;
                    return Ok(StoreResult::Updated {
                        id: best_match.experience.id.clone(),
                        action: "replaced".to_string(),
                    });
                } else {
                    return Ok(StoreResult::Skipped {
                        reason: "Existing experience has equal or better quality".to_string(),
                    });
                }
            } else if similarity > 0.7 {
                self.merge_solution(&best_match.experience.id, solution, tools_used).await?;
                return Ok(StoreResult::Merged {
                    id: best_match.experience.id.clone(),
                });
            }
        }

        let embedding_id = format!("exp_{}", uuid::Uuid::new_v4());

        let experience = Experience::new(
            task_description.to_string(),
            task_type.to_string(),
            task_description.to_string(),
            solution.to_string(),
            tools_used.to_vec(),
            1,
            success,
            embedding_id,
        );

        self.store(&experience).await?;

        let _ = sqlx::query(
            "UPDATE experiences SET quality_score = ? WHERE id = ?",
        )
        .bind(quality_score)
        .bind(&experience.id)
        .execute(&self.pool)
        .await;

        Ok(StoreResult::Created {
            id: experience.id,
        })
    }

    async fn update_solution(&self, id: &str, new_solution: &str, quality_score: f32) -> Result<()> {
        sqlx::query(
            r#"UPDATE experiences
               SET solution_summary = ?, quality_score = ?, iteration_count = iteration_count + 1
               WHERE id = ?"#,
        )
        .bind(new_solution)
        .bind(quality_score)
        .bind(id)
        .execute(&self.pool)
        .await
        .context("Failed to update experience")?;
        Ok(())
    }

    async fn merge_solution(&self, id: &str, new_solution: &str, tools_used: &[String]) -> Result<()> {
        let existing = self.get_by_id(id).await?
            .context("Experience not found")?;

        let merged_solution = format!(
            "{}\n\n---\n\n{}",
            existing.solution_summary, new_solution
        );

        let mut all_tools = existing.tools_used;
        for tool in tools_used {
            if !all_tools.contains(tool) {
                all_tools.push(tool.clone());
            }
        }

        let avg_quality = (existing.quality_score + Experience::compute_quality_score(new_solution)) / 2.0;

        sqlx::query(
            r#"UPDATE experiences
               SET solution_summary = ?, tools_used = ?, quality_score = ?, iteration_count = iteration_count + 1
               WHERE id = ?"#,
        )
        .bind(&merged_solution)
        .bind(serde_json::to_string(&all_tools)?)
        .bind(avg_quality)
        .bind(id)
        .execute(&self.pool)
        .await
        .context("Failed to merge experience")?;
        Ok(())
    }

    async fn increment_recall_count(&self, id: &str) -> Result<()> {
        sqlx::query(
            r#"UPDATE experiences SET recall_count = recall_count + 1 WHERE id = ?"#,
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .context("Failed to increment recall count")?;
        Ok(())
    }

    pub async fn recall(&self, query: &ExperienceQuery) -> Result<Vec<RecalledExperience>> {
        let embedding = self.compute_embedding(&query.query_text).await?;
        let collection_name = &self.config.collection_name;

        let request = QueryPointsBuilder::new(collection_name)
            .limit(query.limit as u64)
            .query(Query::new_nearest(embedding))
            .using("dense")
            .score_threshold(query.score_threshold)
            .with_payload(true);

        let search_result = self.qdrant.query(request).await?;

        let mut recalled = Vec::new();
        for point in search_result.result {
            let experience_id = point
                .payload
                .get("experience_id")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_default();

            if !experience_id.is_empty() {
                if let Some(exp) = self.get_by_id(&experience_id).await? {
                    self.increment_recall_count(&experience_id).await?;
                    recalled.push(RecalledExperience {
                        experience: exp,
                        similarity_score: point.score,
                    });
                }
            }
        }

        debug!(
            "Recalled {} experiences for query: {}",
            recalled.len(),
            query.query_text
        );
        Ok(recalled)
    }

    pub async fn search_by_task_type(
        &self,
        task_type: &str,
        limit: usize,
    ) -> Result<Vec<Experience>> {
        let rows = sqlx::query(
            r#"
            SELECT * FROM experiences
            WHERE task_type = ? AND success = 1
            ORDER BY created_at DESC
            LIMIT ?
            "#,
        )
        .bind(task_type)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("Failed to query experiences by task type")?;

        let mut experiences = Vec::new();
        for row in rows {
            if let Ok(exp) = self.row_to_experience(&row) {
                experiences.push(exp);
            }
        }

        Ok(experiences)
    }

    pub async fn get_by_id(&self, id: &str) -> Result<Option<Experience>> {
        let row = sqlx::query("SELECT * FROM experiences WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .context("Failed to query experience by ID")?;

        match row {
            Some(r) => Ok(Some(self.row_to_experience(&r)?)),
            None => Ok(None),
        }
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        if let Some(_exp) = self.get_by_id(id).await? {
            sqlx::query("DELETE FROM experiences WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await
                .context("Failed to delete experience from SQLite")?;
            info!("Deleted experience from SQLite: {}", id);
        }
        Ok(())
    }

    pub async fn list_ids(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT id FROM experiences ORDER BY created_at DESC")
            .fetch_all(&self.pool)
            .await
            .context("Failed to list experience IDs")?;

        let mut ids = Vec::new();
        for row in rows {
            if let Ok(id) = row.try_get::<String, _>("id") {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    pub async fn count(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) as count FROM experiences")
            .fetch_one(&self.pool)
            .await
            .context("Failed to count experiences")?;

        Ok(row.try_get::<i64, _>("count")?)
    }

    pub async fn list_all(&self) -> Result<Vec<Experience>> {
        let rows = sqlx::query(
            r#"SELECT id, task_description, task_type, problem_summary, solution_summary,
                      tools_used, iteration_count, success, embedding_id, created_at
               FROM experiences ORDER BY created_at DESC"#
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to list experiences")?;

        let mut experiences = Vec::new();
        for row in rows {
            let tools_json = row.try_get::<String, _>("tools_used")?;
            let tools_used: Vec<String> = serde_json::from_str(&tools_json).unwrap_or_default();

            experiences.push(Experience {
                id: row.try_get("id")?,
                task_description: row.try_get("task_description")?,
                task_type: row.try_get("task_type")?,
                problem_summary: row.try_get("problem_summary")?,
                solution_summary: row.try_get("solution_summary")?,
                tools_used,
                iteration_count: row.try_get("iteration_count")?,
                success: row.try_get::<i32, _>("success")? != 0,
                embedding_id: row.try_get("embedding_id")?,
                created_at: chrono::DateTime::from_timestamp(row.try_get::<i64, _>("created_at")?, 0)
                    .unwrap_or_else(chrono::Utc::now),
                quality_score: row.try_get::<f32, _>("quality_score").unwrap_or(0.0),
                recall_count: row.try_get::<i32, _>("recall_count").unwrap_or(0),
            });
        }
        Ok(experiences)
    }

    async fn compute_embedding(&self, text: &str) -> Result<Vec<f32>> {
        let model = self.embedding_model.read().await;
        let query_text = "query: ".to_string() + text;
        let results = model.embed(&[query_text.as_str()], None, None).await?;

        match results.first() {
            Some(EmbeddingResult::DenseVector(v)) => Ok(v.clone()),
            Some(EmbeddingResult::MultiVector(vecs)) => {
                Ok(vecs.iter().flatten().cloned().collect())
            }
            None => Err(anyhow::anyhow!("No embedding returned")),
        }
    }

    fn row_to_experience(&self, row: &SqliteRow) -> Result<Experience> {
        let tools_json: String = row.try_get("tools_used")?;
        let tools_used: Vec<String> = serde_json::from_str(&tools_json)
            .context("Failed to deserialize tools_used")?;

        let created_at: i64 = row.try_get("created_at")?;
        let created_at = chrono::DateTime::from_timestamp(created_at, 0)
            .unwrap_or_else(|| chrono::Utc::now());

        let id: String = row.try_get("id")?;
        let task_description: String = row.try_get("task_description")?;
        let task_type: String = row.try_get("task_type")?;
        let problem_summary: String = row.try_get("problem_summary")?;
        let solution_summary: String = row.try_get("solution_summary")?;
        let iteration_count: i32 = row.try_get("iteration_count")?;
        let success: i32 = row.try_get("success")?;
        let embedding_id: String = row.try_get("embedding_id")?;

        Ok(Experience {
            id,
            task_description,
            task_type,
            problem_summary,
            solution_summary,
            tools_used,
            iteration_count,
            success: success != 0,
            embedding_id,
            created_at,
            quality_score: row.try_get::<f32, _>("quality_score").unwrap_or(0.0),
            recall_count: row.try_get::<i32, _>("recall_count").unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn create_test_config(tmp_dir: &std::path::Path) -> Config {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let config_path = manifest_dir.join("../config.jsonc");

        let mut config = if config_path.exists() {
            Config::from_file(&config_path).unwrap_or_else(|e| {
                tracing::warn!("Failed to load config.jsonc: {}, using defaults", e);
                Config::with_qdrant("http://localhost:6334", None)
            })
        } else {
            Config::with_qdrant("http://localhost:6334", None)
        };

        config.db_path = tmp_dir.join("test_experiences.db");
        config.collection_name = format!("test_experiences_{}", uuid::Uuid::new_v4());

        config
    }

    #[tokio::test]
    async fn test_store_and_retrieve() {
        let tmp_dir = tempdir().unwrap();
        let config = create_test_config(tmp_dir.path());

        let store = ExperienceStore::new(config.clone()).await;
        if store.is_err() {
            tracing::warn!("Skipping test: Qdrant not available");
            return;
        }
        let store = store.unwrap();

        let exp = Experience::new(
            "Fix memory leak".to_string(),
            "bug_fix".to_string(),
            "Memory grew unbounded".to_string(),
            "Added connection pool limits".to_string(),
            vec!["bash".to_string(), "grep".to_string()],
            2,
            true,
            "embedding_1".to_string(),
        );

        store.store(&exp).await.unwrap();

        let retrieved = store.get_by_id(&exp.id).await.unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.task_description, "Fix memory leak");
        assert_eq!(retrieved.task_type, "bug_fix");
    }

    #[tokio::test]
    async fn test_search_by_task_type() {
        let tmp_dir = tempdir().unwrap();
        let config = create_test_config(tmp_dir.path());

        let store = ExperienceStore::new(config.clone()).await;
        if store.is_err() {
            tracing::warn!("Skipping test: Qdrant not available");
            return;
        }
        let store = store.unwrap();

        let exp1 = Experience::new(
            "Bug 1".to_string(),
            "bug_fix".to_string(),
            "Problem 1".to_string(),
            "Solution 1".to_string(),
            vec![],
            1,
            true,
            "embedding_1".to_string(),
        );

        let exp2 = Experience::new(
            "Feature 1".to_string(),
            "feature_impl".to_string(),
            "Problem 2".to_string(),
            "Solution 2".to_string(),
            vec![],
            1,
            true,
            "embedding_2".to_string(),
        );

        store.store(&exp1).await.unwrap();
        store.store(&exp2).await.unwrap();

        let bug_fixes = store.search_by_task_type("bug_fix", 10).await.unwrap();
        assert_eq!(bug_fixes.len(), 1);
        assert_eq!(bug_fixes[0].task_type, "bug_fix");
    }

    #[tokio::test]
    async fn test_count() {
        let tmp_dir = tempdir().unwrap();
        let config = create_test_config(tmp_dir.path());

        let store = ExperienceStore::new(config.clone()).await;
        if store.is_err() {
            tracing::warn!("Skipping test: Qdrant not available");
            return;
        }
        let store = store.unwrap();

        let count = store.count().await.unwrap();
        assert_eq!(count, 0);

        let exp = Experience::new(
            "Test".to_string(),
            "test".to_string(),
            "Problem".to_string(),
            "Solution".to_string(),
            vec![],
            1,
            true,
            "embedding_test".to_string(),
        );

        store.store(&exp).await.unwrap();
        let count = store.count().await.unwrap();
        assert_eq!(count, 1);
    }

    fn make_quality_solution(min_quality: f32) -> String {
        let base_solution = r#"
# Solution

This is a comprehensive solution that addresses the problem.

## Implementation

Here is the code:

```rust
fn main() {
    println!("Hello, world!");
}
```

## Steps

1. First step
2. Second step
3. Third step

## Additional Details

The solution includes proper error handling and follows best practices.
It is well-documented and tested.
"#;

        if min_quality <= 5.0 {
            return base_solution.to_string();
        }

        let mut solution = base_solution.to_string();
        while Experience::compute_quality_score(&solution) < min_quality {
            solution.push_str("\n\n## Extended Analysis\n\n");
            solution.push_str("Additional content to increase quality score. ");
            solution.push_str("This solution provides a thorough approach to the problem. ");
        }
        solution
    }

    fn make_bug_fix_task(variant: u8) -> String {
        match variant {
            0 => "Fix memory leak in connection pool handler".to_string(),
            1 => "Fix memory leak in database connection manager".to_string(),
            2 => "Fix crash when handling concurrent HTTP requests".to_string(),
            3 => "Fix null pointer exception in user authentication".to_string(),
            _ => format!("Fix bug number {}", variant),
        }
    }

    fn make_feature_impl_task(variant: u8) -> String {
        match variant {
            0 => "Implement user authentication with JWT tokens".to_string(),
            1 => "Add rate limiting to API endpoints".to_string(),
            2 => "Create caching layer for database queries".to_string(),
            _ => format!("Implement feature number {}", variant),
        }
    }

    async fn delete_collection_if_exists(store: &ExperienceStore) {
        let collection_name = &store.config.collection_name;
        let _ = store.qdrant.delete_collection(collection_name).await;
    }

    #[tokio::test]
    async fn smart_store_creates_new_when_no_similar() {
        let tmp_dir = tempdir().unwrap();
        let config = create_test_config(tmp_dir.path());

        let store = ExperienceStore::new(config.clone()).await;
        if store.is_err() {
            tracing::warn!("Skipping test: Qdrant not available");
            return;
        }
        let store = store.unwrap();
        let store = Arc::new(store);

        let result = store
            .smart_store(
                "Implement user authentication",
                "feature_impl",
                &make_quality_solution(6.0),
                &["rust".to_string(), "jwt".to_string()],
                true,
            )
            .await
            .unwrap();

        assert!(matches!(result, StoreResult::Created { .. }));

        let count = store.count().await.unwrap();
        assert_eq!(count, 1);

        delete_collection_if_exists(&store).await;
    }

    #[tokio::test]
    async fn smart_store_replaces_when_higher_quality() {
        let tmp_dir = tempdir().unwrap();
        let config = create_test_config(tmp_dir.path());

        let store = ExperienceStore::new(config.clone()).await;
        if store.is_err() {
            tracing::warn!("Skipping test: Qdrant not available");
            return;
        }
        let store = store.unwrap();
        let store = Arc::new(store);

        let task_desc = "Fix memory leak in connection pool";

        let result1 = store
            .smart_store(
                task_desc,
                "bug_fix",
                &make_quality_solution(5.5),
                &["grep".to_string(), "sed".to_string()],
                true,
            )
            .await
            .unwrap();
        assert!(matches!(result1, StoreResult::Created { .. }));

        let result2 = store
            .smart_store(
                task_desc,
                "bug_fix",
                &make_quality_solution(7.0),
                &["rust".to_string(), "profiling".to_string(), "valgrind".to_string()],
                true,
            )
            .await
            .unwrap();

        assert!(matches!(
            result2,
            StoreResult::Updated { id: _, action: ref a } if a == "replaced"
        ));

        let count = store.count().await.unwrap();
        assert_eq!(count, 1);

        let retrieved = store.get_by_id(&result2.id()).await.unwrap().unwrap();
        assert!(retrieved.solution_summary.contains("Extended Analysis"));
        assert_eq!(retrieved.iteration_count, 2);

        delete_collection_if_exists(&store).await;
    }

    #[tokio::test]
    async fn smart_store_skips_when_lower_quality() {
        let tmp_dir = tempdir().unwrap();
        let config = create_test_config(tmp_dir.path());

        let store = ExperienceStore::new(config.clone()).await;
        if store.is_err() {
            tracing::warn!("Skipping test: Qdrant not available");
            return;
        }
        let store = store.unwrap();
        let store = Arc::new(store);

        let task_desc = "Fix memory leak in connection pool";

        let result1 = store
            .smart_store(
                task_desc,
                "bug_fix",
                &make_quality_solution(7.0),
                &["rust".to_string(), "profiling".to_string()],
                true,
            )
            .await
            .unwrap();
        assert!(matches!(result1, StoreResult::Created { .. }));

        let result2 = store
            .smart_store(
                "Fix memory leak in connection pool",
                "bug_fix",
                "Quick fix.",
                &["sed".to_string()],
                true,
            )
            .await
            .unwrap();

        assert!(matches!(
            result2,
            StoreResult::Skipped { reason: ref r } if r.contains("Quality score below threshold")
        ));

        let count = store.count().await.unwrap();
        assert_eq!(count, 1);

        let retrieved = store.get_by_id(&result1.id()).await.unwrap().unwrap();
        assert!(!retrieved.solution_summary.contains("Quick fix"));

        delete_collection_if_exists(&store).await;
    }

    #[tokio::test]
    async fn smart_store_skips_when_equal_quality() {
        let tmp_dir = tempdir().unwrap();
        let config = create_test_config(tmp_dir.path());

        let store = ExperienceStore::new(config.clone()).await;
        if store.is_err() {
            tracing::warn!("Skipping test: Qdrant not available");
            return;
        }
        let store = store.unwrap();
        let store = Arc::new(store);

        let task_desc = make_bug_fix_task(0);

        let solution1 = make_quality_solution(6.0);
        let result1 = store
            .smart_store(&task_desc, "bug_fix", &solution1, &["rust".to_string()], true)
            .await
            .unwrap();
        assert!(matches!(result1, StoreResult::Created { .. }));

        let solution2 = make_quality_solution(5.5);
        let result2 = store
            .smart_store(&task_desc, "bug_fix", &solution2, &["rust".to_string()], true)
            .await
            .unwrap();

        assert!(matches!(
            result2,
            StoreResult::Skipped { reason: ref r } if r.contains("equal or better quality")
        ));

        let count = store.count().await.unwrap();
        assert_eq!(count, 1);

        delete_collection_if_exists(&store).await;
    }

    #[tokio::test]
    async fn smart_store_merges_when_moderate_similarity() {
        let tmp_dir = tempdir().unwrap();
        let config = create_test_config(tmp_dir.path());

        let store = ExperienceStore::new(config.clone()).await;
        if store.is_err() {
            tracing::warn!("Skipping test: Qdrant not available");
            return;
        }
        let store = store.unwrap();
        let store = Arc::new(store);

        let task1 = "Fix memory leak in connection pool handler";
        let result1 = store
            .smart_store(task1, "bug_fix", &make_quality_solution(6.0), &["rust".to_string()], true)
            .await
            .unwrap();
        assert!(matches!(result1, StoreResult::Created { .. }));

        let task2 = "Fix memory leak in database connection manager";
        let result2 = store
            .smart_store(
                task2,
                "bug_fix",
                &make_quality_solution(6.0),
                &["gdb".to_string(), "strace".to_string()],
                true,
            )
            .await
            .unwrap();

        assert!(matches!(result2, StoreResult::Merged { .. }), "Expected Merged, got {:?}", result2);

        let count = store.count().await.unwrap();
        assert_eq!(count, 1);

        let retrieved = store.get_by_id(&result2.id()).await.unwrap().unwrap();
        assert!(retrieved.solution_summary.contains("---"));
        assert!(retrieved.tools_used.contains(&"gdb".to_string()));
        assert!(retrieved.tools_used.contains(&"rust".to_string()));
        assert_eq!(retrieved.iteration_count, 2);

        delete_collection_if_exists(&store).await;
    }

    #[tokio::test]
    async fn smart_store_stores_failed_attempt() {
        let tmp_dir = tempdir().unwrap();
        let config = create_test_config(tmp_dir.path());

        let store = ExperienceStore::new(config.clone()).await;
        if store.is_err() {
            tracing::warn!("Skipping test: Qdrant not available");
            return;
        }
        let store = store.unwrap();
        let store = Arc::new(store);

        let result = store
            .smart_store(
                "Fix critical bug",
                "bug_fix",
                &make_quality_solution(6.0),
                &["debugger".to_string()],
                false,
            )
            .await
            .unwrap();

        assert!(matches!(result, StoreResult::Created { .. }));

        let count = store.count().await.unwrap();
        assert_eq!(count, 1);

        let retrieved = store
            .get_by_id(&result.id())
            .await
            .unwrap()
            .unwrap();
        assert!(!retrieved.success);

        delete_collection_if_exists(&store).await;
    }

    #[tokio::test]
    async fn recall_returns_similar_sorted_by_score() {
        let tmp_dir = tempdir().unwrap();
        let config = create_test_config(tmp_dir.path());

        let store = ExperienceStore::new(config.clone()).await;
        if store.is_err() {
            tracing::warn!("Skipping test: Qdrant not available");
            return;
        }
        let store = store.unwrap();
        let store = Arc::new(store);

        store
            .smart_store(
                &make_bug_fix_task(0),
                "bug_fix",
                &make_quality_solution(6.0),
                &["rust".to_string()],
                true,
            )
            .await
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        store
            .smart_store(
                &make_feature_impl_task(0),
                "feature_impl",
                &make_quality_solution(6.0),
                &["rust".to_string(), "jwt".to_string()],
                true,
            )
            .await
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        store
            .smart_store(
                &make_bug_fix_task(1),
                "bug_fix",
                &make_quality_solution(6.0),
                &["rust".to_string()],
                true,
            )
            .await
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        let query = ExperienceQuery {
            query_text: "Fix crash in web server handler".to_string(),
            task_type_filter: None,
            limit: 5,
            score_threshold: 0.5,
        };

        let recalled = store.recall(&query).await.unwrap();

        assert!(!recalled.is_empty());

        for i in 1..recalled.len() {
            assert!(recalled[i - 1].similarity_score >= recalled[i].similarity_score);
        }

        let bug_fixes: Vec<_> = recalled
            .iter()
            .filter(|r| r.experience.task_type == "bug_fix")
            .collect();
        assert!(!bug_fixes.is_empty());

        delete_collection_if_exists(&store).await;
    }

    #[tokio::test]
    async fn recall_count_incremented() {
        let tmp_dir = tempdir().unwrap();
        let config = create_test_config(tmp_dir.path());

        let store = ExperienceStore::new(config.clone()).await;
        if store.is_err() {
            tracing::warn!("Skipping test: Qdrant not available");
            return;
        }
        let store = store.unwrap();
        let store = Arc::new(store);

        let create_result = store
            .smart_store(
                "Implement caching layer",
                "feature_impl",
                &make_quality_solution(6.0),
                &["redis".to_string(), "rust".to_string()],
                true,
            )
            .await
            .unwrap();

        let id = create_result.id();
        let query = ExperienceQuery {
            query_text: "Implement caching layer".to_string(),
            task_type_filter: None,
            limit: 3,
            score_threshold: 0.5,
        };

        let recalled1 = store.recall(&query).await.unwrap();
        assert!(!recalled1.is_empty());
        assert_eq!(recalled1[0].experience.id, id);

        let after_first = store.get_by_id(id).await.unwrap().unwrap();
        assert_eq!(after_first.recall_count, 1);

        let recalled2 = store.recall(&query).await.unwrap();
        assert!(!recalled2.is_empty());

        let after_second = store.get_by_id(id).await.unwrap().unwrap();
        assert_eq!(after_second.recall_count, 2);

        delete_collection_if_exists(&store).await;
    }
}
