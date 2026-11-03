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

/// One stored message together with its database identity.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    /// Auto-incrementing row id (insertion order).
    pub id: i64,
    /// Human-readable local timestamp of the row.
    pub created_at: String,
    /// The deserialized conversation message.
    pub message: Message,
}

/// Whether the persistent-memory table has been created yet.
///
/// A fresh database has no table until the first agent session is created, so
/// history queries must treat "missing" as "no sessions" rather than error.
async fn memory_table_exists(pool: &SqlitePool) -> Result<bool> {
    let row = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'memory_messages'",
    )
    .fetch_optional(pool)
    .await
    .context("Failed to inspect the memory database schema")?;
    Ok(row.is_some())
}

/// List persisted conversation sessions, newest first.
///
/// Sessions are derived from the `memory_messages` table, so a session exists
/// once it has at least one stored message.
pub async fn list_sessions(pool: &SqlitePool) -> Result<Vec<gewu_protocol::SessionSummary>> {
    if !memory_table_exists(pool).await? {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        r#"
        SELECT
            session_id,
            COUNT(*) AS message_count,
            datetime(MAX(created_at), 'unixepoch', 'localtime') AS updated_at
        FROM memory_messages
        GROUP BY session_id
        ORDER BY MAX(created_at) DESC, session_id ASC
        "#,
    )
    .fetch_all(pool)
    .await
    .context("Failed to list memory sessions; the database may not contain agent memory")?;

    rows.into_iter()
        .map(|row| {
            Ok(gewu_protocol::SessionSummary {
                session_id: row.try_get("session_id")?,
                message_count: row.try_get("message_count")?,
                updated_at: row
                    .try_get::<Option<String>, _>("updated_at")?
                    .unwrap_or_else(|| "unknown".to_string()),
            })
        })
        .collect()
}

/// Delete every stored message of one session.
///
/// Returns the number of removed rows (0 for an unknown session). A fresh
/// database without the memory table simply has nothing to delete.
pub async fn delete_session(pool: &SqlitePool, session_id: &str) -> Result<u64> {
    if !memory_table_exists(pool).await? {
        return Ok(0);
    }
    let result = sqlx::query("DELETE FROM memory_messages WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await
        .with_context(|| format!("Failed to delete session '{}'", session_id))?;
    Ok(result.rows_affected())
}

/// Load every message stored for one session, oldest first.
pub async fn load_session(pool: &SqlitePool, session_id: &str) -> Result<Vec<StoredMessage>> {
    if !memory_table_exists(pool).await? {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        r#"
        SELECT
            id,
            content,
            datetime(created_at, 'unixepoch', 'localtime') AS created_at
        FROM memory_messages
        WHERE session_id = ?
        ORDER BY id ASC
        "#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .with_context(|| format!("Failed to load memory for session '{}'", session_id))?;

    rows.into_iter()
        .map(|row| {
            let id: i64 = row.try_get("id")?;
            let content: String = row.try_get("content")?;
            let message = serde_json::from_str(&content)
                .with_context(|| format!("Failed to deserialize memory message {}", id))?;
            Ok(StoredMessage {
                id,
                created_at: row
                    .try_get::<Option<String>, _>("created_at")?
                    .unwrap_or_else(|| "unknown".to_string()),
                message,
            })
        })
        .collect()
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

    #[tokio::test]
    async fn lists_and_loads_sessions() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        sqlx::query(
            r#"
            CREATE TABLE memory_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await?;

        let user = Message::User {
            content: vec![ContentPart::Text {
                text: "first".to_string(),
            }],
        };
        let assistant = Message::Assistant {
            content: Some("second".to_string()),
            tool_calls: None,
        };
        for (session, message, at) in [
            ("older", &user, 1),
            ("newer", &user, 2),
            ("newer", &assistant, 3),
        ] {
            sqlx::query(
                "INSERT INTO memory_messages (session_id, content, created_at) VALUES (?, ?, ?)",
            )
            .bind(session)
            .bind(serde_json::to_string(message)?)
            .bind(at)
            .execute(&pool)
            .await?;
        }

        let sessions = list_sessions(&pool).await?;
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "newer");
        assert_eq!(sessions[0].message_count, 2);
        assert_eq!(sessions[1].session_id, "older");

        let stored = load_session(&pool, "newer").await?;
        assert_eq!(stored.len(), 2);
        assert!(stored[0].id < stored[1].id);
        assert_eq!(
            serde_json::to_value(&stored[0].message)?,
            serde_json::to_value(&user)?
        );
        assert_eq!(
            serde_json::to_value(&stored[1].message)?,
            serde_json::to_value(&assistant)?
        );

        assert!(load_session(&pool, "missing").await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn deletes_a_session() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        sqlx::query(
            r#"
            CREATE TABLE memory_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await?;

        let user = Message::User {
            content: vec![ContentPart::Text {
                text: "doomed".to_string(),
            }],
        };
        for (session, at) in [("gone", 1), ("kept", 2)] {
            sqlx::query(
                "INSERT INTO memory_messages (session_id, content, created_at) VALUES (?, ?, ?)",
            )
            .bind(session)
            .bind(serde_json::to_string(&user)?)
            .bind(at)
            .execute(&pool)
            .await?;
        }

        assert_eq!(delete_session(&pool, "gone").await?, 1);
        assert_eq!(delete_session(&pool, "gone").await?, 0, "already gone");

        let remaining: Vec<String> = list_sessions(&pool)
            .await?
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(remaining, vec!["kept"]);
        Ok(())
    }

    #[tokio::test]
    async fn deleting_from_a_fresh_database_is_a_noop() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        assert_eq!(delete_session(&pool, "anything").await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn a_fresh_database_has_no_sessions() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        assert!(list_sessions(&pool).await?.is_empty());
        assert!(load_session(&pool, "anything").await?.is_empty());
        Ok(())
    }
}
