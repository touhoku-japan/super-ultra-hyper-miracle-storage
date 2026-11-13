CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY
);
INSERT OR IGNORE INTO schema_migrations (version) VALUES (1);

CREATE TABLE IF NOT EXISTS disks (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL,
    path TEXT NOT NULL,
    online INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS buckets (
    name TEXT PRIMARY KEY,
    replicas INTEGER,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS objects (
    id TEXT PRIMARY KEY,
    bucket TEXT NOT NULL REFERENCES buckets (name),
    object_key TEXT NOT NULL,
    size INTEGER NOT NULL,
    etag TEXT NOT NULL,
    blake3 TEXT NOT NULL,
    content_type TEXT,
    storage_class TEXT,
    desired_replicas INTEGER NOT NULL,
    replicas_explicit INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (bucket, object_key)
);

CREATE TABLE IF NOT EXISTS replicas (
    object_id TEXT NOT NULL REFERENCES objects (id) ON DELETE CASCADE,
    disk_id TEXT NOT NULL,
    PRIMARY KEY (object_id, disk_id)
);
CREATE INDEX IF NOT EXISTS replicas_by_disk ON replicas (disk_id);
