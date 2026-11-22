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
use gewu_core::traits::{Memory, SessionStateStore};
use gewu_core::types::Message;
use sqlx::{Row, sqlite::SqlitePool};
use tracing::{error, info};

async fn ensure_reverts(pool: &SqlitePool) -> Result<()> {
    sqlx::query("CREATE TABLE IF NOT EXISTS chat_reverts (session_id TEXT PRIMARY KEY, messages TEXT NOT NULL)").execute(pool).await?;
    // Reminders became durable after chat_reverts existed; add the column
    // best-effort so older databases keep working.
    let _ = sqlx::query("ALTER TABLE chat_reverts ADD COLUMN reminder_state TEXT")
        .execute(pool)
        .await;
    Ok(())
}

async fn ensure_reminders(pool: &SqlitePool) -> Result<()> {
    sqlx::query("CREATE TABLE IF NOT EXISTS session_reminders (session_id TEXT PRIMARY KEY, state TEXT NOT NULL)").execute(pool).await?;
    Ok(())
}

async fn ensure_models(pool: &SqlitePool) -> Result<()> {
    sqlx::query("CREATE TABLE IF NOT EXISTS session_models (session_id TEXT PRIMARY KEY, model TEXT NOT NULL)").execute(pool).await?;
    Ok(())
}

async fn ensure_agents(pool: &SqlitePool) -> Result<()> {
    sqlx::query("CREATE TABLE IF NOT EXISTS session_agents (session_id TEXT PRIMARY KEY, agent TEXT NOT NULL)").execute(pool).await?;
    Ok(())
}

async fn ensure_workspaces(pool: &SqlitePool) -> Result<()> {
    sqlx::query("CREATE TABLE IF NOT EXISTS session_workspaces (session_id TEXT PRIMARY KEY, workspace TEXT NOT NULL)").execute(pool).await?;
    Ok(())
}

async fn ensure_reasoning_efforts(pool: &SqlitePool) -> Result<()> {
    sqlx::query("CREATE TABLE IF NOT EXISTS session_reasoning_efforts (session_id TEXT PRIMARY KEY, effort TEXT NOT NULL)").execute(pool).await?;
    Ok(())
}

async fn ensure_session_state(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS session_state (\
            session_id TEXT NOT NULL, \
            key TEXT NOT NULL, \
            value TEXT NOT NULL, \
            PRIMARY KEY (session_id, key)\
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The durable per-session reminder state, or `None` when the session has
/// never persisted one.
pub async fn get_reminder_opt(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Option<gewu_core::agent::Reminder>> {
    ensure_reminders(pool).await?;
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM session_reminders WHERE session_id = ?")
            .bind(session_id)
            .fetch_optional(pool)
            .await?;
    Ok(state.and_then(|text| serde_json::from_str(&text).ok()))
}

/// Persist the per-session reminder state (upsert).
pub async fn set_reminder(
    pool: &SqlitePool,
    session_id: &str,
    reminder: &gewu_core::agent::Reminder,
) -> Result<()> {
    ensure_reminders(pool).await?;
    sqlx::query("INSERT INTO session_reminders (session_id, state) VALUES (?, ?) ON CONFLICT(session_id) DO UPDATE SET state = excluded.state")
        .bind(session_id).bind(serde_json::to_string(reminder)?).execute(pool).await?;
    Ok(())
}

/// Marks sessions created by the `task` tool so the session list can hide
/// them: a subagent transcript is real data (task resume needs it) but is
/// not a top-level conversation.
async fn ensure_subagents(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS subagent_sessions (\
            session_id TEXT PRIMARY KEY, \
            parent_session_id TEXT NOT NULL, \
            description TEXT NOT NULL DEFAULT '', \
            created_at INTEGER DEFAULT (unixepoch())\
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Record `session_id` as a subagent spawned by `parent_session_id`.
///
/// Idempotent: resuming a task id keeps its original parent row.
pub async fn record_subagent_session(
    pool: &SqlitePool,
    session_id: &str,
    parent_session_id: &str,
    description: &str,
) -> Result<()> {
    ensure_subagents(pool).await?;
    sqlx::query(
        "INSERT OR IGNORE INTO subagent_sessions(session_id, parent_session_id, description) \
         VALUES (?, ?, ?)",
    )
    .bind(session_id)
    .bind(parent_session_id)
    .bind(description)
    .execute(pool)
    .await?;
    Ok(())
}

/// Number of subagent ancestors of a session: 0 for a top-level conversation,
/// 1 for a direct subagent. Used to cap nested subagent spawning. Cycle-capped
/// defensively; a well-formed table never loops.
pub async fn subagent_depth(pool: &SqlitePool, session_id: &str) -> Result<usize> {
    ensure_subagents(pool).await?;
    let mut depth = 0;
    let mut current = session_id.to_string();
    while depth < 64 {
        let parent: Option<String> = sqlx::query_scalar(
            "SELECT parent_session_id FROM subagent_sessions WHERE session_id = ?",
        )
        .bind(&current)
        .fetch_optional(pool)
        .await?;
        let Some(parent) = parent else { break };
        current = parent;
        depth += 1;
    }
    Ok(depth)
}

/// Every transitive subagent session of `session_id`, order not guaranteed.
async fn subagent_descendants(pool: &SqlitePool, session_id: &str) -> Result<Vec<String>> {
    ensure_subagents(pool).await?;
    let mut all = Vec::new();
    let mut frontier = vec![session_id.to_string()];
    while let Some(id) = frontier.pop() {
        let children: Vec<(String,)> =
            sqlx::query_as("SELECT session_id FROM subagent_sessions WHERE parent_session_id = ?")
                .bind(&id)
                .fetch_all(pool)
                .await?;
        for (child,) in children {
            if child != session_id && !all.contains(&child) {
                frontier.push(child.clone());
                all.push(child);
            }
        }
    }
    Ok(all)
}

#[cfg(test)]
mod revert_tests {
    use super::*;

    #[tokio::test]
    async fn revert_restore_and_commit_are_persistent() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let mut memory = SqliteMemory::new(pool.clone(), "chat".into())
            .await
            .unwrap();
        for text in ["first", "second"] {
            memory
                .add_message(Message::User {
                    content: vec![gewu_core::types::ContentPart::Text { text: text.into() }],
                })
                .await;
        }
        let original = memory.messages().to_vec();
        let reminder = gewu_core::agent::Reminder::switch("FULL", Some("SPARSE".into()));
        assert!(
            revert_chat(&pool, "chat", Some(9), false, Some(&reminder))
                .await
                .is_err()
        );
        let (reverted, restored) = revert_chat(&pool, "chat", None, false, Some(&reminder))
            .await
            .unwrap();
        assert_eq!(reverted, original[..1]);
        assert!(
            restored.is_none(),
            "forward reverts never restore a reminder"
        );
        assert_eq!(
            SqliteMemory::new(pool.clone(), "chat".into())
                .await
                .unwrap()
                .messages()
                .len(),
            1
        );
        let (restored_messages, restored_reminder) =
            revert_chat(&pool, "chat", None, true, None).await.unwrap();
        assert_eq!(restored_messages, original);
        assert_eq!(
            restored_reminder.as_ref().unwrap().texts(),
            reminder.texts()
        );
        assert!(revert_chat(&pool, "chat", None, true, None).await.is_err());
        revert_chat(&pool, "chat", Some(0), false, Some(&reminder))
            .await
            .unwrap();
        commit_chat_revert(&pool, "chat").await.unwrap();
        assert!(revert_chat(&pool, "chat", None, true, None).await.is_err());
    }

    #[tokio::test]
    async fn reminder_state_round_trips_through_sql() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        assert!(get_reminder_opt(&pool, "sess").await.unwrap().is_none());
        let reminder = gewu_core::agent::Reminder::switch("FULL", None);
        set_reminder(&pool, "sess", &reminder).await.unwrap();
        let stored = get_reminder_opt(&pool, "sess").await.unwrap().unwrap();
        assert_eq!(stored.texts(), reminder.texts());
        // Upsert replaces the state.
        let updated = gewu_core::agent::Reminder::continue_at("FULL", Some("SPARSE".into()), 7);
        set_reminder(&pool, "sess", &updated).await.unwrap();
        let stored = get_reminder_opt(&pool, "sess").await.unwrap().unwrap();
        assert_eq!(stored.texts(), updated.texts());
    }
}

pub async fn commit_chat_revert(pool: &SqlitePool, session_id: &str) -> Result<()> {
    ensure_reverts(pool).await?;
    sqlx::query("DELETE FROM chat_reverts WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Revert to the zero-based user turn, or restore the last staged revert.
/// Only conversation rows are touched, never workspace files.
///
/// The reminder state is snapshotted alongside the messages and returned
/// restored on undo; on a forward revert the caller recomputes it from the
/// retained history and persists it separately.
pub async fn revert_chat(
    pool: &SqlitePool,
    session_id: &str,
    turn: Option<usize>,
    undo: bool,
    reminder: Option<&gewu_core::agent::Reminder>,
) -> Result<(Vec<Message>, Option<gewu_core::agent::Reminder>)> {
    ensure_reverts(pool).await?;
    let mut tx = pool.begin().await?;
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT content FROM memory_messages WHERE session_id = ? ORDER BY id")
            .bind(session_id)
            .fetch_all(&mut *tx)
            .await?;
    let current: Vec<Message> = rows
        .iter()
        .map(|(text,)| serde_json::from_str(text))
        .collect::<std::result::Result<_, _>>()?;
    let (next, restored_reminder) = if undo {
        let saved: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT messages, reminder_state FROM chat_reverts WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (saved_messages, saved_reminder) =
            saved.ok_or_else(|| anyhow::anyhow!("no chat revert to undo"))?;
        let next: Vec<Message> = serde_json::from_str(&saved_messages)?;
        let restored = saved_reminder.and_then(|text| serde_json::from_str(&text).ok());
        (next, restored)
    } else {
        let users: Vec<usize> = current
            .iter()
            .enumerate()
            .filter_map(|(i, m)| matches!(m, Message::User { .. }).then_some(i))
            .collect();
        let target = turn.unwrap_or_else(|| users.len().saturating_sub(1));
        let boundary = *users
            .get(target)
            .ok_or_else(|| anyhow::anyhow!("user turn not found"))?;
        sqlx::query("INSERT INTO chat_reverts(session_id, messages, reminder_state) VALUES (?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET messages = excluded.messages, reminder_state = excluded.reminder_state")
            .bind(session_id).bind(serde_json::to_string(&current)?).bind(reminder.map(serde_json::to_string).transpose()?).execute(&mut *tx).await?;
        (current[..boundary].to_vec(), None)
    };
    sqlx::query("DELETE FROM memory_messages WHERE session_id = ?")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    for message in &next {
        sqlx::query("INSERT INTO memory_messages(session_id, content) VALUES (?, ?)")
            .bind(session_id)
            .bind(serde_json::to_string(message)?)
            .execute(&mut *tx)
            .await?;
    }
    if undo {
        sqlx::query("DELETE FROM chat_reverts WHERE session_id = ?")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok((next, restored_reminder))
}

/// Session profile metadata is separate from message history (legacy sessions
/// without a row continue to use Build).
pub async fn session_agent(pool: &SqlitePool, session_id: &str) -> Result<String> {
    ensure_agents(pool).await?;
    let value: Option<String> =
        sqlx::query_scalar("SELECT agent FROM session_agents WHERE session_id = ?")
            .bind(session_id)
            .fetch_optional(pool)
            .await?;
    Ok(value.unwrap_or_else(|| "build".into()))
}

pub async fn set_session_agent(pool: &SqlitePool, session_id: &str, agent: &str) -> Result<()> {
    session_agent(pool, session_id).await?;
    sqlx::query("INSERT INTO session_agents(session_id, agent) VALUES (?, ?) ON CONFLICT(session_id) DO UPDATE SET agent = excluded.agent").bind(session_id).bind(agent).execute(pool).await?;
    Ok(())
}

/// Session model metadata: the `endpoint/model` selector the conversation
/// runs on. `None` for legacy sessions (they use the server default).
pub async fn session_model(pool: &SqlitePool, session_id: &str) -> Result<Option<String>> {
    ensure_models(pool).await?;
    let value: Option<String> =
        sqlx::query_scalar("SELECT model FROM session_models WHERE session_id = ?")
            .bind(session_id)
            .fetch_optional(pool)
            .await?;
    Ok(value)
}

pub async fn set_session_model(pool: &SqlitePool, session_id: &str, model: &str) -> Result<()> {
    session_model(pool, session_id).await?;
    sqlx::query("INSERT INTO session_models(session_id, model) VALUES (?, ?) ON CONFLICT(session_id) DO UPDATE SET model = excluded.model").bind(session_id).bind(model).execute(pool).await?;
    Ok(())
}

/// Session workspace metadata. `None` means a legacy session that uses `cwd`.
pub async fn session_workspace(pool: &SqlitePool, session_id: &str) -> Result<Option<String>> {
    ensure_workspaces(pool).await?;
    Ok(
        sqlx::query_scalar("SELECT workspace FROM session_workspaces WHERE session_id = ?")
            .bind(session_id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn set_session_workspace(
    pool: &SqlitePool,
    session_id: &str,
    workspace: &str,
) -> Result<()> {
    ensure_workspaces(pool).await?;
    sqlx::query("INSERT INTO session_workspaces(session_id, workspace) VALUES (?, ?) ON CONFLICT(session_id) DO UPDATE SET workspace = excluded.workspace")
        .bind(session_id)
        .bind(workspace)
        .execute(pool)
        .await?;
    Ok(())
}

/// Session reasoning-effort metadata. `None` means the model's configured
/// default (legacy sessions included).
pub async fn session_reasoning_effort(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Option<String>> {
    ensure_reasoning_efforts(pool).await?;
    Ok(
        sqlx::query_scalar("SELECT effort FROM session_reasoning_efforts WHERE session_id = ?")
            .bind(session_id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn set_session_reasoning_effort(
    pool: &SqlitePool,
    session_id: &str,
    effort: Option<&str>,
) -> Result<()> {
    ensure_reasoning_efforts(pool).await?;
    match effort {
        Some(effort) => {
            sqlx::query("INSERT INTO session_reasoning_efforts(session_id, effort) VALUES (?, ?) ON CONFLICT(session_id) DO UPDATE SET effort = excluded.effort")
                .bind(session_id)
                .bind(effort)
                .execute(pool)
                .await?;
        }
        None => {
            sqlx::query("DELETE FROM session_reasoning_efforts WHERE session_id = ?")
                .bind(session_id)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

/// SQLite-backed [`SessionStateStore`] for per-session tool state.
///
/// Values are JSON strings keyed by `(session_id, key)`; tools access them
/// through [`gewu_core::types::SessionState`] handles on their tool context.
/// The backing table is created lazily on first use.
pub struct SqliteSessionStateStore {
    pool: SqlitePool,
}

impl SqliteSessionStateStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SessionStateStore for SqliteSessionStateStore {
    async fn load(&self, session_id: &str, key: &str) -> Result<Option<String>> {
        ensure_session_state(&self.pool).await?;
        let value: Option<String> =
            sqlx::query_scalar("SELECT value FROM session_state WHERE session_id = ? AND key = ?")
                .bind(session_id)
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        Ok(value)
    }

    async fn save(&self, session_id: &str, key: &str, value: String) -> Result<()> {
        ensure_session_state(&self.pool).await?;
        sqlx::query(
            "INSERT INTO session_state (session_id, key, value) VALUES (?, ?, ?) \
             ON CONFLICT(session_id, key) DO UPDATE SET value = excluded.value",
        )
        .bind(session_id)
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

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
/// Sessions start with a stored message or a persisted application binding.
/// Subagent sessions recorded via
/// [`record_subagent_session`] are excluded: they belong to their parent
/// conversation, not the session list.
pub async fn list_sessions(pool: &SqlitePool) -> Result<Vec<gewu_protocol::SessionSummary>> {
    session_agent(pool, "").await?;
    ensure_subagents(pool).await?;
    ensure_session_state(pool).await?;
    if !memory_table_exists(pool).await? {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        r#"
        WITH activity AS (
            SELECT session_id, COUNT(*) AS message_count, MAX(created_at) AS time
            FROM memory_messages GROUP BY session_id
            UNION ALL
            SELECT session_id, 0, CAST(value AS INTEGER) FROM session_state WHERE key = 'gewu.plugin.session'
        )
        SELECT activity.session_id,
            COALESCE((SELECT agent FROM session_agents WHERE session_agents.session_id = activity.session_id), 'build') AS agent,
            SUM(activity.message_count) AS message_count,
            datetime(MAX(activity.time), 'unixepoch', 'localtime') AS updated_at
        FROM activity
        LEFT JOIN subagent_sessions ON subagent_sessions.session_id = activity.session_id
        WHERE subagent_sessions.session_id IS NULL
        GROUP BY activity.session_id
        ORDER BY MAX(activity.time) DESC, activity.session_id ASC
        "#,
    )
    .fetch_all(pool)
    .await
    .context("Failed to list memory sessions; the database may not contain agent memory")?;

    rows.into_iter()
        .map(|row| {
            Ok(gewu_protocol::SessionSummary {
                agent: row.try_get("agent")?,
                session_id: row.try_get("session_id")?,
                message_count: row.try_get("message_count")?,
                updated_at: row
                    .try_get::<Option<String>, _>("updated_at")?
                    .unwrap_or_else(|| "unknown".to_string()),
            })
        })
        .collect()
}

/// Delete every stored message of one session, including the transcripts of
/// any subagent sessions spawned from it (directly or transitively), plus
/// their subagent marker rows.
///
/// Returns the number of removed message rows (0 for an unknown session). A
/// fresh database without the memory table simply has nothing to delete.
pub async fn delete_session(pool: &SqlitePool, session_id: &str) -> Result<u64> {
    if !memory_table_exists(pool).await? {
        return Ok(0);
    }
    ensure_subagents(pool).await?;
    // The cascade below touches every per-session side table; fresh
    // databases may not have them yet, so create them first.
    ensure_reminders(pool).await?;
    ensure_models(pool).await?;
    ensure_agents(pool).await?;
    ensure_workspaces(pool).await?;
    ensure_reasoning_efforts(pool).await?;
    ensure_reverts(pool).await?;
    ensure_session_state(pool).await?;
    let descendants = subagent_descendants(pool, session_id).await?;
    let mut removed = sqlx::query("DELETE FROM memory_messages WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await
        .with_context(|| format!("Failed to delete session '{}'", session_id))?
        .rows_affected();
    for id in &descendants {
        removed += sqlx::query("DELETE FROM memory_messages WHERE session_id = ?")
            .bind(id)
            .execute(pool)
            .await?
            .rows_affected();
    }
    // Drop every marker related to the deleted subtree: the subtree's own
    // rows, and the session's marker if it was itself someone's subagent.
    for id in std::iter::once(session_id).chain(descendants.iter().map(String::as_str)) {
        sqlx::query("DELETE FROM subagent_sessions WHERE session_id = ? OR parent_session_id = ?")
            .bind(id)
            .bind(id)
            .execute(pool)
            .await?;
        // Per-session side tables (reminders, model/agent metadata, staged
        // reverts, tool state) are cleared too so deleting a conversation
        // leaves no orphan rows behind.
        for table in [
            "session_reminders",
            "session_models",
            "session_agents",
            "session_workspaces",
            "session_reasoning_efforts",
            "chat_reverts",
            "session_state",
        ] {
            sqlx::query(&format!("DELETE FROM {table} WHERE session_id = ?"))
                .bind(id)
                .execute(pool)
                .await?;
        }
    }
    Ok(removed)
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
    async fn add_checkpoint(&mut self, summary: String, recent: String) -> anyhow::Result<()> {
        let message = Message::Compaction { summary, recent };
        let content = serde_json::to_string(&message)?;
        sqlx::query("INSERT INTO memory_messages (session_id, content) VALUES (?, ?)")
            .bind(&self.session_id)
            .bind(content)
            .execute(&self.pool)
            .await?;
        self.messages.push(message);
        Ok(())
    }
    fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Adds a message to the database and then to the in-memory cache.
    ///
    /// Persistence happens first: if serializing or writing the row fails,
    /// the message is dropped from both the database and the cache and the
    /// failure is logged. This keeps the cache a faithful copy of durable
    /// history.
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
    /// Used for explicit history edits, not compaction. The operation
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
    async fn plugin_session_restores_without_chat_and_deletes_cleanly() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        SqliteMemory::new(pool.clone(), "app-only".into()).await?;
        let store = SqliteSessionStateStore::new(pool.clone());
        store
            .save("app-only", "gewu.plugin.session", "123".into())
            .await?;
        let sessions = list_sessions(&pool).await?;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "app-only");
        assert_eq!(sessions[0].message_count, 0);
        assert_eq!(delete_session(&pool, "app-only").await?, 0);
        assert!(list_sessions(&pool).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn plugin_session_markers_do_not_double_count_chat_or_expose_subagents() -> Result<()> {
        use gewu_core::traits::Memory;
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        let mut memory = SqliteMemory::new(pool.clone(), "chat".into()).await?;
        memory
            .add_message(Message::User {
                content: vec![ContentPart::Text {
                    text: "hello".into(),
                }],
            })
            .await;
        let store = SqliteSessionStateStore::new(pool.clone());
        for session in ["chat", "child", "app-only"] {
            store
                .save(session, "gewu.plugin.session", "123".into())
                .await?;
        }
        record_subagent_session(&pool, "child", "chat", "explore").await?;
        let sessions = list_sessions(&pool).await?;
        assert_eq!(sessions.len(), 2);
        assert_eq!(
            sessions
                .iter()
                .find(|s| s.session_id == "chat")
                .unwrap()
                .message_count,
            1
        );
        assert_eq!(
            sessions
                .iter()
                .find(|s| s.session_id == "app-only")
                .unwrap()
                .message_count,
            0
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_model_round_trips() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        assert_eq!(session_model(&pool, "s1").await?, None);
        set_session_model(&pool, "s1", "west/coding").await?;
        assert_eq!(
            session_model(&pool, "s1").await?.as_deref(),
            Some("west/coding")
        );
        set_session_model(&pool, "s1", "east/m2").await?;
        assert_eq!(
            session_model(&pool, "s1").await?.as_deref(),
            Some("east/m2")
        );
        assert_eq!(session_model(&pool, "other").await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn session_reasoning_effort_round_trips() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        assert_eq!(session_reasoning_effort(&pool, "s1").await?, None);
        set_session_reasoning_effort(&pool, "s1", Some("high")).await?;
        assert_eq!(
            session_reasoning_effort(&pool, "s1").await?.as_deref(),
            Some("high")
        );
        set_session_reasoning_effort(&pool, "s1", None).await?;
        assert_eq!(session_reasoning_effort(&pool, "s1").await?, None);
        assert_eq!(session_reasoning_effort(&pool, "other").await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn session_workspace_round_trips() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        assert_eq!(session_workspace(&pool, "s1").await?, None);
        set_session_workspace(&pool, "s1", "frontend").await?;
        assert_eq!(
            session_workspace(&pool, "s1").await?.as_deref(),
            Some("frontend")
        );
        set_session_workspace(&pool, "s1", "backend").await?;
        assert_eq!(
            session_workspace(&pool, "s1").await?.as_deref(),
            Some("backend")
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_state_store_round_trips_and_isolates_keys() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        let store = SqliteSessionStateStore::new(pool.clone());
        assert_eq!(store.load("s1", "todos").await?, None);
        store.save("s1", "todos", r#"[{"id":"1"}]"#.into()).await?;
        assert_eq!(
            store.load("s1", "todos").await?.as_deref(),
            Some(r#"[{"id":"1"}]"#)
        );
        store.save("s1", "todos", "[]".into()).await?;
        assert_eq!(store.load("s1", "todos").await?.as_deref(), Some("[]"));
        assert_eq!(store.load("s2", "todos").await?, None, "other session");
        assert_eq!(store.load("s1", "other").await?, None, "other key");
        Ok(())
    }

    #[tokio::test]
    async fn deleting_a_session_clears_its_side_tables() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        let user = |text: &str| Message::User {
            content: vec![ContentPart::Text { text: text.into() }],
        };
        SqliteMemory::new(pool.clone(), "main".into()).await?;
        for text in ["hello", "again"] {
            sqlx::query("INSERT INTO memory_messages (session_id, content) VALUES (?, ?)")
                .bind("main")
                .bind(serde_json::to_string(&user(text))?)
                .execute(&pool)
                .await?;
        }
        set_reminder(&pool, "main", &gewu_core::agent::Reminder::default()).await?;
        set_session_model(&pool, "main", "east/m1").await?;
        set_session_agent(&pool, "main", "build").await?;
        set_session_workspace(&pool, "main", "frontend").await?;
        set_session_reasoning_effort(&pool, "main", Some("high")).await?;
        let store = SqliteSessionStateStore::new(pool.clone());
        store.save("main", "todos", "[]".into()).await?;
        revert_chat(&pool, "main", None, false, None).await?;

        assert_eq!(delete_session(&pool, "main").await?, 1);
        assert!(get_reminder_opt(&pool, "main").await?.is_none());
        for table in [
            "session_models",
            "session_agents",
            "session_workspaces",
            "session_reasoning_efforts",
            "chat_reverts",
        ] {
            let count: i64 = sqlx::query_scalar(&format!(
                "SELECT COUNT(*) FROM {table} WHERE session_id = 'main'"
            ))
            .fetch_one(&pool)
            .await?;
            assert_eq!(count, 0, "{table} must be empty after delete");
        }
        assert_eq!(store.load("main", "todos").await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn revert_and_undo_restore_compaction_context() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        let mut memory = SqliteMemory::new(pool.clone(), "boundaries".into()).await?;
        let user = |text: &str| Message::User {
            content: vec![ContentPart::Text { text: text.into() }],
        };
        memory.add_message(user("first")).await;
        memory.add_message(user("second")).await;
        memory
            .add_checkpoint("summary".into(), "tail".into())
            .await?;
        memory.add_message(user("third")).await;
        let original = memory.messages().to_vec();
        let original_context = gewu_core::compaction::active_context(&original).to_messages();
        let (retained, _) = revert_chat(&pool, "boundaries", Some(1), false, None).await?;
        assert_eq!(retained, vec![user("first")]);
        let loaded = SqliteMemory::new(pool.clone(), "boundaries".into()).await?;
        assert_eq!(
            gewu_core::compaction::active_context(loaded.messages()).to_messages(),
            retained
        );
        let (restored, _) = revert_chat(&pool, "boundaries", None, true, None).await?;
        assert_eq!(restored, original);
        let loaded = SqliteMemory::new(pool.clone(), "boundaries".into()).await?;
        assert_eq!(
            gewu_core::compaction::active_context(loaded.messages()).to_messages(),
            original_context
        );
        Ok(())
    }

    #[tokio::test]
    async fn compaction_preserves_original_rows_and_reloads_context() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        let mut memory = SqliteMemory::new(pool.clone(), "compact".into()).await?;
        let original = Message::Tool {
            tool_call_id: "call".into(),
            tool_name: None,
            content: "original".repeat(10_000),
            media: vec![],
        };
        memory.add_message(original.clone()).await;
        let original_id: i64 =
            sqlx::query_scalar("SELECT id FROM memory_messages WHERE session_id = 'compact'")
                .fetch_one(&pool)
                .await?;
        memory
            .add_checkpoint("first".into(), "recent".into())
            .await?;
        memory
            .add_checkpoint("latest".into(), "tail".into())
            .await?;
        let loaded = SqliteMemory::new(pool.clone(), "compact".into()).await?;
        assert_eq!(loaded.messages().len(), 3);
        assert_eq!(loaded.messages()[0], original);
        let retained_id: i64 =
            sqlx::query_scalar("SELECT MIN(id) FROM memory_messages WHERE session_id = 'compact'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(original_id, retained_id);
        assert_eq!(
            gewu_core::compaction::active_context(loaded.messages()).to_messages(),
            vec![gewu_core::compaction::build_checkpoint_message(
                "latest", "tail"
            )]
        );
        pool.close().await;
        assert!(
            memory
                .add_checkpoint("failed".into(), "".into())
                .await
                .is_err()
        );
        assert_eq!(
            memory.messages().len(),
            3,
            "failed persistence cannot activate a boundary"
        );
        Ok(())
    }

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
    async fn reminder_parts_round_trip_with_the_message() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        let mut memory = SqliteMemory::new(pool.clone(), "reminders".into()).await?;
        memory
            .add_message(Message::User {
                content: vec![
                    ContentPart::Text {
                        text: "Update README.md".into(),
                    },
                    ContentPart::Reminder {
                        text: "Plan mode is active".into(),
                    },
                ],
            })
            .await;
        let reloaded = SqliteMemory::new(pool, "reminders".into()).await?;
        assert_eq!(reloaded.messages(), memory.messages());
        Ok(())
    }

    #[tokio::test]
    async fn persists_assistant_reasoning() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        let session_id = "test_reasoning".to_string();

        let mut mem = SqliteMemory::new(pool.clone(), session_id.clone()).await?;
        mem.add_message(Message::Assistant {
            content: Some("answer".to_string()),
            reasoning: Some("step by step".to_string()),
            replay: Some(gewu_core::types::ReasoningReplay {
                origin: "anthropic:https://api.anthropic.com".into(),
                model: "claude-test".into(),
                field: None,
                blocks: vec![serde_json::json!({"type":"thinking","thinking":"step by step","signature":"sig"})],
            }),
            tool_calls: None,
        })
        .await;
        mem.add_message(Message::Assistant {
            content: Some("plain".to_string()),
            reasoning: None,
            replay: None,
            tool_calls: None,
        })
        .await;

        let reloaded = SqliteMemory::new(pool, session_id).await?;
        assert_eq!(reloaded.messages().len(), 2);
        match &reloaded.messages()[0] {
            Message::Assistant {
                content,
                reasoning,
                replay,
                tool_calls,
            } => {
                assert_eq!(content.as_deref(), Some("answer"));
                assert_eq!(reasoning.as_deref(), Some("step by step"));
                assert_eq!(replay.as_ref().unwrap().blocks[0]["signature"], "sig");
                assert!(tool_calls.is_none());
            }
            _ => panic!("Wrong type"),
        }
        match &reloaded.messages()[1] {
            Message::Assistant { reasoning, .. } => assert!(reasoning.is_none()),
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
            reasoning: None,
            replay: None,
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

    #[tokio::test]
    async fn subagent_sessions_are_hidden_from_the_list_but_still_loadable() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        let user = Message::User {
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
        };
        for session in ["main", "task-a", "task-b"] {
            SqliteMemory::new(pool.clone(), session.into())
                .await
                .unwrap();
            sqlx::query("INSERT INTO memory_messages (session_id, content) VALUES (?, ?)")
                .bind(session)
                .bind(serde_json::to_string(&user)?)
                .execute(&pool)
                .await?;
        }
        record_subagent_session(&pool, "task-a", "main", "explore").await?;
        record_subagent_session(&pool, "task-b", "main", "explore").await?;
        // Recording again (task resume) keeps the original parent row.
        record_subagent_session(&pool, "task-a", "other", "explore").await?;

        let listed: Vec<String> = list_sessions(&pool)
            .await?
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(listed, vec!["main"]);

        // Subagent transcripts remain reachable for task resume.
        assert_eq!(load_session(&pool, "task-a").await?.len(), 1);
        assert_eq!(subagent_depth(&pool, "main").await?, 0);
        assert_eq!(subagent_depth(&pool, "task-a").await?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn deleting_a_session_cascades_to_its_subagents() -> Result<()> {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
        let user = Message::User {
            content: vec![ContentPart::Text {
                text: "doomed".to_string(),
            }],
        };
        for session in ["root", "child", "grandchild", "unrelated"] {
            SqliteMemory::new(pool.clone(), session.into())
                .await
                .unwrap();
            sqlx::query("INSERT INTO memory_messages (session_id, content) VALUES (?, ?)")
                .bind(session)
                .bind(serde_json::to_string(&user)?)
                .execute(&pool)
                .await?;
        }
        record_subagent_session(&pool, "child", "root", "explore").await?;
        record_subagent_session(&pool, "grandchild", "child", "explore").await?;

        assert_eq!(delete_session(&pool, "root").await?, 3);
        let remaining: Vec<String> = list_sessions(&pool)
            .await?
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(remaining, vec!["unrelated"]);
        assert!(load_session(&pool, "grandchild").await?.is_empty());
        Ok(())
    }
}
