CREATE TABLE IF NOT EXISTS execution_events (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(run_id) REFERENCES execution_runs(id)
);

CREATE INDEX IF NOT EXISTS idx_execution_events_run ON execution_events(run_id);
CREATE INDEX IF NOT EXISTS idx_execution_events_type ON execution_events(event_type);
CREATE INDEX IF NOT EXISTS idx_execution_events_created ON execution_events(created_at);
