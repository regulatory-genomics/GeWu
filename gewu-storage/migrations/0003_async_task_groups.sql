-- Async task groups: parent session + message id pair, members table for child tasks.
CREATE TABLE IF NOT EXISTS async_task_groups (
    group_id TEXT PRIMARY KEY,
    parent_session_id TEXT NOT NULL,
    parent_message_id TEXT NOT NULL,
    expected_member_count INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'running',
    injected_at INTEGER,
    created_at INTEGER DEFAULT (unixepoch()),
    updated_at INTEGER DEFAULT (unixepoch()),
    UNIQUE(parent_session_id, parent_message_id)
);

CREATE TABLE IF NOT EXISTS async_task_group_members (
    group_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    child_session_id TEXT NOT NULL,
    created_at INTEGER DEFAULT (unixepoch()),
    PRIMARY KEY(group_id, task_id),
    FOREIGN KEY(group_id) REFERENCES async_task_groups(group_id)
);

CREATE INDEX IF NOT EXISTS idx_atg_parent ON async_task_groups(parent_session_id, parent_message_id);
CREATE INDEX IF NOT EXISTS idx_atgm_task ON async_task_group_members(task_id);
CREATE INDEX IF NOT EXISTS idx_atgm_group ON async_task_group_members(group_id);
