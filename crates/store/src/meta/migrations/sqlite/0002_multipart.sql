CREATE TABLE multipart_uploads (
    id TEXT PRIMARY KEY,
    bucket TEXT NOT NULL REFERENCES buckets (name),
    object_key TEXT NOT NULL,
    content_type TEXT,
    storage_class TEXT,
    user_metadata TEXT NOT NULL,
    desired_replicas INTEGER NOT NULL,
    replicas_explicit INTEGER NOT NULL,
    -- JSON array of the disk ids chosen for this upload's parts
    disks TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX multipart_uploads_by_key ON multipart_uploads (bucket, object_key, id);

CREATE TABLE multipart_parts (
    upload_id TEXT NOT NULL REFERENCES multipart_uploads (id) ON DELETE CASCADE,
    part_number INTEGER NOT NULL,
    blob_id TEXT NOT NULL,
    size INTEGER NOT NULL,
    etag TEXT NOT NULL,
    blake3 TEXT NOT NULL,
    -- JSON array of the disk ids holding this part
    disks TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (upload_id, part_number)
);
