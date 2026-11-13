-- Versioning: several rows per key, one of them the latest. SQLite cannot drop the old
-- UNIQUE (bucket, object_key), so the table is rebuilt (the runner disables foreign keys
-- around this migration so replicas are not cascade-deleted).
CREATE TABLE objects_v2 (
    id TEXT PRIMARY KEY,
    bucket TEXT NOT NULL REFERENCES buckets (name),
    object_key TEXT NOT NULL,
    version_id TEXT NOT NULL,
    is_latest BOOLEAN NOT NULL,
    delete_marker BOOLEAN NOT NULL,
    size INTEGER NOT NULL,
    etag TEXT NOT NULL,
    blake3 TEXT NOT NULL,
    content_type TEXT,
    storage_class TEXT,
    user_metadata TEXT NOT NULL,
    desired_replicas INTEGER NOT NULL,
    replicas_explicit BOOLEAN NOT NULL,
    created_at INTEGER NOT NULL,
    noncurrent_at INTEGER,
    lock_mode TEXT,
    lock_until INTEGER,
    legal_hold BOOLEAN NOT NULL DEFAULT FALSE,
    UNIQUE (bucket, object_key, version_id)
);
INSERT INTO objects_v2 (id, bucket, object_key, version_id, is_latest, delete_marker, size, etag,
    blake3, content_type, storage_class, user_metadata, desired_replicas, replicas_explicit,
    created_at)
SELECT id, bucket, object_key, 'null', TRUE, FALSE, size, etag, blake3, content_type,
    storage_class, user_metadata, desired_replicas, replicas_explicit, created_at
FROM objects;
DROP TABLE objects;
ALTER TABLE objects_v2 RENAME TO objects;
CREATE UNIQUE INDEX objects_latest ON objects (bucket, object_key) WHERE is_latest;
CREATE INDEX objects_versions ON objects (bucket, object_key, created_at);

ALTER TABLE buckets ADD COLUMN versioning TEXT;
ALTER TABLE buckets ADD COLUMN lifecycle TEXT;
ALTER TABLE buckets ADD COLUMN cors TEXT;
ALTER TABLE buckets ADD COLUMN object_lock TEXT;
