-- Repair backoff: objects that could not be repaired wait before the next attempt
-- (10 minutes, doubling up to 6 hours) instead of being retried on every pass.
-- (No partial index for the checksum backfill on MariaDB: it scans objects by id.)
ALTER TABLE repair_queue ADD COLUMN attempts INT NOT NULL DEFAULT 0;
ALTER TABLE repair_queue ADD COLUMN retry_at BIGINT NULL;
