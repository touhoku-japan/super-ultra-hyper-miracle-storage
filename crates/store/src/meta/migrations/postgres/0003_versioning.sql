ALTER TABLE objects ADD COLUMN version_id TEXT NOT NULL DEFAULT 'null';
ALTER TABLE objects ADD COLUMN is_latest BOOLEAN NOT NULL DEFAULT TRUE;
ALTER TABLE objects ADD COLUMN delete_marker BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE objects ADD COLUMN noncurrent_at BIGINT;
ALTER TABLE objects ADD COLUMN lock_mode TEXT;
ALTER TABLE objects ADD COLUMN lock_until BIGINT;
ALTER TABLE objects ADD COLUMN legal_hold BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE objects DROP CONSTRAINT objects_bucket_object_key_key;
ALTER TABLE objects ADD CONSTRAINT objects_version_unique UNIQUE (bucket, object_key, version_id);
CREATE UNIQUE INDEX objects_latest ON objects (bucket, object_key) WHERE is_latest;
CREATE INDEX objects_versions ON objects (bucket, object_key, created_at);

ALTER TABLE buckets ADD COLUMN versioning TEXT;
ALTER TABLE buckets ADD COLUMN lifecycle TEXT;
ALTER TABLE buckets ADD COLUMN cors TEXT;
ALTER TABLE buckets ADD COLUMN object_lock TEXT;
