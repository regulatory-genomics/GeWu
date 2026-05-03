CREATE TABLE IF NOT EXISTS execution_runs (
    id TEXT PRIMARY KEY,
    parent_id TEXT,
    session_id TEXT,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    status TEXT NOT NULL,
    input_json TEXT,
    output_json TEXT,
    error TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER
);

CREATE INDEX IF NOT EXISTS idx_execution_runs_session ON execution_runs(session_id);
CREATE INDEX IF NOT EXISTS idx_execution_runs_parent ON execution_runs(parent_id);
CREATE INDEX IF NOT EXISTS idx_execution_runs_status ON execution_runs(status);
CREATE INDEX IF NOT EXISTS idx_execution_runs_started ON execution_runs(started_at);
