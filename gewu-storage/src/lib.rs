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
use sqlx::{Row, sqlite::SqlitePool};
use tracing::{error, info};

pub mod async_task_group_store;
pub mod execution_trace_store;
pub mod session_store;
pub mod task_store;
pub use async_task_group_store::{AsyncTaskGroup, AsyncTaskGroupStore};
pub use execution_trace_store::{ExecutionTraceStore, StartExecutionRun};
pub use session_store::SessionStore;
pub use task_store::TaskStore;

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
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS memory_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER DEFAULT (unixepoch())
            )
            "#,
        )
        .execute(&pool)
        .await
        .context("Failed to create memory_messages table")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memory_messages_session_id ON memory_messages(session_id)"
        )
        .execute(&pool)
        .await
        .context("Failed to create index on session_id")?;

        let rows =
            sqlx::query("SELECT content FROM memory_messages WHERE session_id = ? ORDER BY id ASC")
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
    /// The message is:
    /// 1. Added to the in-memory vector immediately
    /// 2. Asynchronously persisted to SQLite
    ///
    /// If serialization fails, the in-memory cache is updated but
    /// the database write fails silently (logged as error).
    async fn add_message(&mut self, message: Message) {
        let session_id = self.session_id.clone();
        let pool = self.pool.clone();

        let content = match serde_json::to_string(&message) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to serialize message for persistence: {}", e);
                return;
            }
        };

        if let Err(e) =
            sqlx::query("INSERT INTO memory_messages (session_id, content) VALUES (?, ?)")
                .bind(session_id)
                .bind(content)
                .execute(&pool)
                .await
        {
            error!("Failed to persist message to DB: {}", e);
            return;
        }

        self.messages.push(message);
    }

    /// Clears all messages for this session from memory and database.
    async fn clear(&mut self) {
        if let Err(e) = sqlx::query("DELETE FROM memory_messages WHERE session_id = ?")
            .bind(&self.session_id)
            .execute(&self.pool)
            .await
        {
            error!("Failed to clear messages from DB: {}", e);
            return;
        }
        self.messages.clear();
    }

    /// Replaces all messages with a new set.
    ///
    /// This is used by the agent's compaction logic to replace the
    /// full conversation history with a summarized version. The operation
    /// is transactional:
    ///
    /// 1. Deletes all existing messages for this session
    /// 2. Inserts all messages from the new vector
    ///
    /// If an error occurs mid-transaction, changes are rolled back
    /// and an error is logged.
    async fn replace(&mut self, messages: Vec<Message>) {
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                error!("Failed to begin transaction for replace: {}", e);
                return;
            }
        };

        if let Err(e) = sqlx::query("DELETE FROM memory_messages WHERE session_id = ?")
            .bind(&self.session_id)
            .execute(&mut *tx)
            .await
        {
            error!("Failed to delete old messages in replace: {}", e);
            return;
        }

        let mut insert_failed = false;
        for msg in &messages {
            let content = match serde_json::to_string(&msg) {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to serialize message in replace: {}", e);
                    insert_failed = true;
                    continue;
                }
            };

            if let Err(e) =
                sqlx::query("INSERT INTO memory_messages (session_id, content) VALUES (?, ?)")
                    .bind(&self.session_id)
                    .bind(content)
                    .execute(&mut *tx)
                    .await
            {
                error!("Failed to insert message in replace: {}", e);
                insert_failed = true;
            }
        }

        if insert_failed {
            return;
        }

        if let Err(e) = tx.commit().await {
            error!("Failed to commit replace transaction: {}", e);
            return;
        }

        self.messages = messages.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_core::types::{ContentPart, Message};
    use sqlx::sqlite::SqlitePoolOptions;

    #[tokio::test]
    async fn test_persistence() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;

        let session_id = "test_1".to_string();

        let mut mem1 = SqliteMemory::new(pool.clone(), session_id.clone()).await?;
        let msg = Message::User {
            content: vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
        };
        mem1.add_message(msg).await;
        assert_eq!(mem1.messages().len(), 1);

        let mem2 = SqliteMemory::new(pool.clone(), session_id.clone()).await?;
        assert_eq!(mem2.messages().len(), 1);
        match &mem2.messages()[0] {
            Message::User { content } => {
                let text = content
                    .iter()
                    .find_map(|p| match p {
                        ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .unwrap_or("");
                assert_eq!(text, "Hello");
            }
            _ => panic!("Wrong type"),
        }

        Ok(())
    }
}
