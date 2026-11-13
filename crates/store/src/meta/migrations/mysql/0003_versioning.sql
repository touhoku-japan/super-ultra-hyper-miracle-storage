ALTER TABLE objects ADD COLUMN version_id VARCHAR(64) NOT NULL DEFAULT 'null';
ALTER TABLE objects ADD COLUMN is_latest BOOLEAN NOT NULL DEFAULT TRUE;
ALTER TABLE objects ADD COLUMN delete_marker BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE objects ADD COLUMN noncurrent_at BIGINT NULL;
ALTER TABLE objects ADD COLUMN lock_mode VARCHAR(16) NULL;
ALTER TABLE objects ADD COLUMN lock_until BIGINT NULL;
ALTER TABLE objects ADD COLUMN legal_hold BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE objects ADD COLUMN latest_key VARBINARY(1024) AS (IF(is_latest, object_key, NULL)) PERSISTENT;
ALTER TABLE objects ADD UNIQUE KEY objects_by_version (bucket, object_key, version_id);
ALTER TABLE objects ADD UNIQUE KEY objects_latest (bucket, latest_key);
ALTER TABLE objects DROP INDEX objects_by_key;
CREATE INDEX objects_versions ON objects (bucket, object_key, created_at);

ALTER TABLE buckets ADD COLUMN versioning VARCHAR(16) NULL;
ALTER TABLE buckets ADD COLUMN lifecycle TEXT NULL;
ALTER TABLE buckets ADD COLUMN cors TEXT NULL;
ALTER TABLE buckets ADD COLUMN object_lock TEXT NULL
