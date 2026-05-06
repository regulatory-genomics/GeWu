CREATE TABLE IF NOT EXISTS active_turn_snapshots (
    session_id TEXT PRIMARY KEY,
    started_at INTEGER NOT NULL,
    last_event_at INTEGER NOT NULL,
    event_count INTEGER NOT NULL,
    reasoning TEXT NOT NULL DEFAULT '',
    content TEXT NOT NULL DEFAULT ''
);
