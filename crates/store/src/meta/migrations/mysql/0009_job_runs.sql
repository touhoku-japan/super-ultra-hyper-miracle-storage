-- The last run of each maintenance job on each node (scheduled or started from the operator
-- console), so the console of any node shows the whole cluster. Each node writes only its own rows.
CREATE TABLE job_runs (
    node_id VARCHAR(128) NOT NULL,
    job VARCHAR(64) NOT NULL,
    running BOOLEAN NOT NULL,
    -- "console" or "schedule".
    started_by VARCHAR(16) NOT NULL,
    -- Unix ms.
    started_at BIGINT NOT NULL,
    finished_at BIGINT NULL,
    -- Unix ms of the last sign of life while running (a crashed node stops updating it).
    heartbeat_at BIGINT NOT NULL,
    -- JSON report of a finished run, or why it failed.
    result TEXT NULL,
    error TEXT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
    PRIMARY KEY (node_id, job)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;
