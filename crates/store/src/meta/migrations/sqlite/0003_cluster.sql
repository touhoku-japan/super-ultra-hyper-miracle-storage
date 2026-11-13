-- Nodes report each disk's free space and liveness; disks whose heartbeat is stale are treated
-- as offline by placement and repair.
ALTER TABLE disks ADD COLUMN free_bytes INTEGER NOT NULL DEFAULT 0;
ALTER TABLE disks ADD COLUMN heartbeat_at INTEGER NOT NULL DEFAULT 0;
