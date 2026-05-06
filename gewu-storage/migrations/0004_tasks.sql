-- Tasks table covering both task identity and execution lifecycle.
CREATE TABLE IF NOT EXISTS tasks (
    task_id TEXT PRIMARY KEY,
    specialist_id TEXT NOT NULL,
    prompt TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'ready',
    execution_status TEXT NOT NULL DEFAULT 'pending',
    result_status TEXT NOT NULL DEFAULT 'pending',
    attempt INTEGER NOT NULL DEFAULT 0,
    block_reason TEXT,
    result_json TEXT,
    finish_reason TEXT,
    error TEXT,
    parent_session_id TEXT,
    child_session_id TEXT,
    root_run_id TEXT,
    started_at INTEGER,
    finished_at INTEGER,
    created_at INTEGER DEFAULT (unixepoch()),
    updated_at INTEGER DEFAULT (unixepoch())
);
