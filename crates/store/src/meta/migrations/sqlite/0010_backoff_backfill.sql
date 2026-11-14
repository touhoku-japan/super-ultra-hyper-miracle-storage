-- Repair backoff: objects that could not be repaired wait before the next attempt
-- (10 minutes, doubling up to 6 hours) instead of being retried on every pass.
ALTER TABLE repair_queue ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE repair_queue ADD COLUMN retry_at INTEGER NULL;

-- Objects stored before checksums were kept by default, for the checksum backfill.
CREATE INDEX objects_without_checksum ON objects (id) WHERE checksum IS NULL AND delete_marker = FALSE;
