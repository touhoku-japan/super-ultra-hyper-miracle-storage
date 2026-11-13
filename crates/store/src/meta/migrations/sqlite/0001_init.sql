CREATE TABLE tenants (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);

CREATE TABLE access_keys (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    secret_enc TEXT NOT NULL,
    active INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX access_keys_by_tenant ON access_keys (tenant_id);

CREATE TABLE disks (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL,
    path TEXT NOT NULL,
    online INTEGER NOT NULL
);

CREATE TABLE buckets (
    name TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES tenants (id),
    replicas INTEGER,
    created_at INTEGER NOT NULL
);
CREATE INDEX buckets_by_owner ON buckets (owner);

CREATE TABLE objects (
    id TEXT PRIMARY KEY,
    bucket TEXT NOT NULL REFERENCES buckets (name),
    object_key TEXT NOT NULL,
    size INTEGER NOT NULL,
    etag TEXT NOT NULL,
    blake3 TEXT NOT NULL,
    content_type TEXT,
    storage_class TEXT,
    user_metadata TEXT NOT NULL,
    desired_replicas INTEGER NOT NULL,
    replicas_explicit INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (bucket, object_key)
);

CREATE TABLE replicas (
    object_id TEXT NOT NULL REFERENCES objects (id) ON DELETE CASCADE,
    disk_id TEXT NOT NULL,
    PRIMARY KEY (object_id, disk_id)
);
CREATE INDEX replicas_by_disk ON replicas (disk_id);
