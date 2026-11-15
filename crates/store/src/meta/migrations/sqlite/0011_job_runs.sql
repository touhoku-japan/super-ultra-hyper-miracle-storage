-- The last run of each maintenance job on each node (scheduled or started from the operator
-- console), so the console of any node shows the whole cluster. Each node writes only its own rows.
CREATE TABLE job_runs (
    node_id TEXT NOT NULL,
    job TEXT NOT NULL,
    running BOOLEAN NOT NULL,
    -- "console" or "schedule".
    started_by TEXT NOT NULL,
    -- Unix ms.
    started_at INTEGER NOT NULL,
    finished_at INTEGER NULL,
    -- Unix ms of the last sign of life while running (a crashed node stops updating it).
    heartbeat_at INTEGER NOT NULL,
    -- JSON report of a finished run, or why it failed.
    result TEXT NULL,
    error TEXT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (node_id, job)
);
CREATE TRIGGER job_runs_updated_at AFTER UPDATE ON job_runs FOR EACH ROW
WHEN NEW.updated_at IS OLD.updated_at
BEGIN
    UPDATE job_runs SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
