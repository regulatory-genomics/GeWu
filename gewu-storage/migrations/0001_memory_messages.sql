-- Initial memory_messages schema
CREATE TABLE IF NOT EXISTS memory_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    content TEXT NOT NULL,
    created_at INTEGER DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_memory_messages_session_id ON memory_messages(session_id);
