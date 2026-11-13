CREATE TABLE tenants (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    created_at BIGINT NOT NULL
);

CREATE TABLE access_keys (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    secret_enc TEXT NOT NULL,
    active BOOLEAN NOT NULL,
    created_at BIGINT NOT NULL
);
CREATE INDEX access_keys_by_tenant ON access_keys (tenant_id);

CREATE TABLE disks (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL,
    path TEXT NOT NULL,
    online BOOLEAN NOT NULL,
    free_bytes BIGINT NOT NULL DEFAULT 0,
    heartbeat_at BIGINT NOT NULL DEFAULT 0
);

CREATE TABLE buckets (
    name TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES tenants (id),
    replicas BIGINT,
    created_at BIGINT NOT NULL
);
CREATE INDEX buckets_by_owner ON buckets (owner);

-- Keys sort by UTF-8 bytes, as in S3: hence COLLATE "C".
CREATE TABLE objects (
    id TEXT PRIMARY KEY,
    bucket TEXT NOT NULL REFERENCES buckets (name),
    object_key TEXT COLLATE "C" NOT NULL,
    size BIGINT NOT NULL,
    etag TEXT NOT NULL,
    blake3 TEXT NOT NULL,
    content_type TEXT,
    storage_class TEXT,
    user_metadata TEXT NOT NULL,
    desired_replicas BIGINT NOT NULL,
    replicas_explicit BOOLEAN NOT NULL,
    created_at BIGINT NOT NULL,
    UNIQUE (bucket, object_key)
);

CREATE TABLE replicas (
    object_id TEXT NOT NULL REFERENCES objects (id) ON DELETE CASCADE,
    disk_id TEXT NOT NULL,
    PRIMARY KEY (object_id, disk_id)
);
CREATE INDEX replicas_by_disk ON replicas (disk_id);

CREATE TABLE multipart_uploads (
    id TEXT PRIMARY KEY,
    bucket TEXT NOT NULL REFERENCES buckets (name),
    object_key TEXT COLLATE "C" NOT NULL,
    content_type TEXT,
    storage_class TEXT,
    user_metadata TEXT NOT NULL,
    desired_replicas BIGINT NOT NULL,
    replicas_explicit BOOLEAN NOT NULL,
    disks TEXT NOT NULL,
    created_at BIGINT NOT NULL
);
CREATE INDEX multipart_uploads_by_key ON multipart_uploads (bucket, object_key, id);

CREATE TABLE multipart_parts (
    upload_id TEXT NOT NULL REFERENCES multipart_uploads (id) ON DELETE CASCADE,
    part_number BIGINT NOT NULL,
    blob_id TEXT NOT NULL,
    size BIGINT NOT NULL,
    etag TEXT NOT NULL,
    blake3 TEXT NOT NULL,
    disks TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (upload_id, part_number)
);
