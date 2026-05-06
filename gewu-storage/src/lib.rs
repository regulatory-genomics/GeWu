//! SQLite-based persistent memory storage for GeWu.
//!
//! This module provides [`SqliteMemory`], a [`Memory`] implementation that
//! persists conversation history to a SQLite database. This enables
//! multi-session support where the agent can resume conversations across
//! process restarts.
//!
//! # Database Schema
//!
//! The `memory_messages` table stores serialized messages:
//!
//! ```sql
//! CREATE TABLE memory_messages (
//!     id INTEGER PRIMARY KEY AUTOINCREMENT,
//!     session_id TEXT NOT NULL,
//!     content TEXT NOT NULL,
//!     created_at INTEGER DEFAULT (unixepoch())
//! )
//! ```
//!
//! - `session_id`: Groups messages into conversation sessions
//! - `content`: JSON-serialized [`Message`] object
//! - `created_at`: Unix timestamp for ordering
//!
//! # Usage
//!
//! ```ignore
//! use gewu_storage::SqliteMemory;
//! use sqlx::sqlite::SqlitePoolOptions;
//!
//! let pool = SqlitePoolOptions::new()
//!     .connect("sqlite:./gewu.db?mode=rwc")
//!     .await
//!     .unwrap();
//!
//! let mut memory = SqliteMemory::new(pool, "session-123")
//!     .await
//!     .unwrap();
//! ```
//!
//! # Concurrency
//!
//! SqliteMemory maintains an in-memory cache of messages for fast access
//! during a session. writes are immediately persisted to the database.

use anyhow::{Context, Result};
use async_trait::async_trait;
use gewu_core::traits::Memory;
use gewu_core::types::Message;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{ConnectOptions, Row, sqlite::SqlitePool};
use std::error::Error;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;
use tracing::info;

pub mod async_task_group_store;
pub mod execution_trace_store;
pub mod project_store;
pub mod session_store;
pub mod task_store;
pub use async_task_group_store::{AsyncTaskGroup, AsyncTaskGroupStore};
pub use execution_trace_store::{ExecutionTraceStore, StartExecutionRun};
pub use project_store::{ProjectRecord, ProjectStore};
pub use session_store::{SessionRecord, SessionStore};
pub use task_store::TaskStore;

/// Versioned SQL migrations (Phase 1, C2). Replaces the inline
/// `CREATE TABLE IF NOT EXISTS` calls scattered across each store.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Run all SQL migrations against the given pool. Idempotent.
pub async fn run_migrations(pool: &SqlitePool) -> Result<()> {
    MIGRATOR
        .run(pool)
        .await
        .context("Failed to run SQL migrations")?;
    Ok(())
}

/// Build a SQLite connection pool tuned for GeWu's workload (Phase 1, C5).
///
/// Sets WAL journal mode, busy_timeout=5s, foreign_keys=ON,
/// synchronous=NORMAL, temp_store=memory, and grows max_connections to 20.
pub async fn create_pool(db_path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = db_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create db parent dir {}", parent.display()))?;
    }

    let url = format!("sqlite://{}", db_path.display());
    let opts = SqliteConnectOptions::from_str(&url)
        .context("parse sqlite url")?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true)
        .pragma("temp_store", "memory")
        .disable_statement_logging();

    let pool = SqlitePoolOptions::new()
        .max_connections(20)
        .connect_with(opts)
        .await
        .context("connect SQLite pool")?;

    Ok(pool)
}

/// SQLite-backed implementation of [`Memory`] for persistent storage.
///
/// SqliteMemory stores conversation messages in a SQLite database, allowing
/// conversations to persist across process restarts. Each instance is
/// associated with a session ID, enabling multiple concurrent conversations.
///
/// The implementation uses a dual-storage approach:
/// - In-memory cache for fast reads during the session
/// - SQLite database for durability and cross-session persistence
///
/// # Type Requirements
///
/// Requires `sqlx` with the `sqlite` feature enabled:
/// ```toml
/// sqlx = { version = "0.7", features = ["runtime-tokio", "sqlite"] }
/// ```
pub struct SqliteMemory {
    pool: SqlitePool,
    session_id: String,
    messages: Vec<Message>,
}

impl SqliteMemory {
    /// Creates a new `SqliteMemory` instance for a given session.
    ///
    /// This function:
    /// 1. Ensures the `memory_messages` table exists in the database
    /// 2. Creates an index on `session_id` for efficient queries
    /// 3. Loads any existing messages for the given `session_id`
    ///
    /// # Arguments
    ///
    /// * `pool` - A SQLx SQLite connection pool
    /// * `session_id` - Unique identifier for this conversation session
    ///
    /// # Errors
    ///
    /// Returns an error if database operations fail (table creation,
    /// index creation, or loading messages).
    pub async fn new(pool: SqlitePool, session_id: String) -> Result<Self> {
        let rows = sqlx::query(
            "SELECT content FROM memory_messages WHERE session_id = ? ORDER BY created_at, id",
        )
        .bind(&session_id)
        .fetch_all(&pool)
        .await
        .context("Failed to load messages from DB")?;

        let mut messages = Vec::new();
        for row in rows {
            let content: String = row.try_get("content")?;
            let message: Message =
                serde_json::from_str(&content).context("Failed to deserialize message content")?;
            messages.push(message);
        }

        info!(
            "Loaded {} messages for session {}",
            messages.len(),
            session_id
        );

        Ok(Self {
            pool,
            session_id,
            messages,
        })
    }
}

#[async_trait]
impl Memory for SqliteMemory {
    fn messages(&self) -> &[Message] {
        &self.messages
    }

    fn messages_mut(&mut self) -> &mut [Message] {
        &mut self.messages
    }

    /// Adds a message to both the in-memory cache and the database.
    ///
    /// The message is persisted to SQLite first; on success it is appended
    /// to the in-memory vector. On failure the error is propagated to the
    /// caller and the in-memory cache is unchanged.
    async fn add_message(
        &mut self,
        message: Message,
    ) -> std::result::Result<(), Box<dyn Error + Send + Sync>> {
        let session_id = self.session_id.clone();
        let pool = self.pool.clone();

        let content = serde_json::to_string(&message)
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

        sqlx::query("INSERT INTO memory_messages (session_id, content) VALUES (?, ?)")
            .bind(session_id)
            .bind(content)
            .execute(&pool)
            .await
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

        self.messages.push(message);
        Ok(())
    }

    /// Clears all messages for this session from memory and database.
    async fn clear(&mut self) -> std::result::Result<(), Box<dyn Error + Send + Sync>> {
        sqlx::query("DELETE FROM memory_messages WHERE session_id = ?")
            .bind(&self.session_id)
            .execute(&self.pool)
            .await
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
        self.messages.clear();
        Ok(())
    }

    /// Replaces all messages with a new set.
    ///
    /// Runs in a single transaction. If any insert fails the transaction
    /// is rolled back and the in-memory cache is unchanged.
    async fn replace(
        &mut self,
        messages: Vec<Message>,
    ) -> std::result::Result<(), Box<dyn Error + Send + Sync>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

        sqlx::query("DELETE FROM memory_messages WHERE session_id = ?")
            .bind(&self.session_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

        for msg in &messages {
            let content = serde_json::to_string(msg)
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

            sqlx::query("INSERT INTO memory_messages (session_id, content) VALUES (?, ?)")
                .bind(&self.session_id)
                .bind(content)
                .execute(&mut *tx)
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
        }

        tx.commit()
            .await
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

        self.messages = messages;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_core::types::{ContentPart, Message};
    use sqlx::sqlite::SqlitePoolOptions;

    async fn make_pool() -> Result<SqlitePool> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        run_migrations(&pool).await?;
        Ok(pool)
    }

    #[tokio::test]
    async fn test_persistence() -> Result<()> {
        let pool = make_pool().await?;

        let session_id = "test_1".to_string();

        let mut mem1 = SqliteMemory::new(pool.clone(), session_id.clone()).await?;
        let msg = Message::User {
            content: vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
        };
        mem1.add_message(msg)
            .await
            .map_err(|e| anyhow::anyhow!("add_message: {}", e))?;
        assert_eq!(mem1.messages().len(), 1);

        let mem2 = SqliteMemory::new(pool.clone(), session_id.clone()).await?;
        assert_eq!(mem2.messages().len(), 1);
        assert!(matches!(
            &mem2.messages()[0],
            Message::User { content } if content.iter().any(|p| matches!(p, ContentPart::Text { text } if text == "Hello"))
        ));

        Ok(())
    }

    #[tokio::test]
    async fn add_message_propagates_db_errors() -> Result<()> {
        let pool = make_pool().await?;
        let mut mem = SqliteMemory::new(pool.clone(), "s".into()).await?;

        // Close the pool out from under the memory; subsequent inserts must error.
        pool.close().await;

        let result = mem
            .add_message(Message::User {
                content: vec![ContentPart::Text { text: "x".into() }],
            })
            .await;
        assert!(
            result.is_err(),
            "expected error on closed pool, got Ok with messages={}",
            mem.messages().len()
        );
        // Cache must not advance on failure.
        assert_eq!(mem.messages().len(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn migrations_runner_is_idempotent() -> Result<()> {
        let pool = make_pool().await?;
        run_migrations(&pool).await?;
        run_migrations(&pool).await?;
        Ok(())
    }
}
