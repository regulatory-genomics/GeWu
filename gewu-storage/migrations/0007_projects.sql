CREATE TABLE IF NOT EXISTS projects (
    project_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    workdir TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(workdir)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_projects_workdir ON projects(workdir);
CREATE INDEX IF NOT EXISTS idx_projects_updated ON projects(updated_at);
