-- Sessions table with project_id and workdir already merged in.
CREATE TABLE IF NOT EXISTS sessions (
    session_id  TEXT PRIMARY KEY,
    agent_type  TEXT NOT NULL,
    project_id  TEXT,
    workdir     TEXT,
    created_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at);
CREATE INDEX IF NOT EXISTS idx_sessions_project_updated ON sessions(project_id, updated_at DESC);

-- Backfill from any pre-existing sessions table that lacked these columns.
-- This is harmless on a fresh install: SQLite raises a duplicate-column error
-- which our runner swallows for backwards compatibility with installs created
-- before migrations existed.
