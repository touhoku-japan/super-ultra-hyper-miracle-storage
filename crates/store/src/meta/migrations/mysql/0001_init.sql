CREATE TABLE tenants (
    id VARCHAR(36) PRIMARY KEY,
    name VARCHAR(128) NOT NULL UNIQUE,
    created_at BIGINT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

CREATE TABLE access_keys (
    id VARCHAR(64) PRIMARY KEY,
    tenant_id VARCHAR(36) NOT NULL,
    secret_enc TEXT NOT NULL,
    active BOOLEAN NOT NULL,
    created_at BIGINT NOT NULL,
    INDEX access_keys_by_tenant (tenant_id),
    FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

CREATE TABLE disks (
    id VARCHAR(36) PRIMARY KEY,
    node_id VARCHAR(128) NOT NULL,
    path TEXT NOT NULL,
    online BOOLEAN NOT NULL,
    free_bytes BIGINT NOT NULL DEFAULT 0,
    heartbeat_at BIGINT NOT NULL DEFAULT 0
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

CREATE TABLE buckets (
    name VARCHAR(63) PRIMARY KEY,
    owner VARCHAR(36) NOT NULL,
    replicas BIGINT NULL,
    created_at BIGINT NOT NULL,
    INDEX buckets_by_owner (owner),
    FOREIGN KEY (owner) REFERENCES tenants (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

-- object_key is VARBINARY: S3 byte order, and it fits InnoDB's 3072-byte index limit.
CREATE TABLE objects (
    id VARCHAR(36) PRIMARY KEY,
    bucket VARCHAR(63) NOT NULL,
    object_key VARBINARY(1024) NOT NULL,
    size BIGINT NOT NULL,
    etag VARCHAR(64) NOT NULL,
    blake3 VARCHAR(64) NOT NULL,
    content_type TEXT NULL,
    storage_class VARCHAR(64) NULL,
    user_metadata TEXT NOT NULL,
    desired_replicas BIGINT NOT NULL,
    replicas_explicit BOOLEAN NOT NULL,
    created_at BIGINT NOT NULL,
    UNIQUE KEY objects_by_key (bucket, object_key),
    FOREIGN KEY (bucket) REFERENCES buckets (name)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

CREATE TABLE replicas (
    object_id VARCHAR(36) NOT NULL,
    disk_id VARCHAR(36) NOT NULL,
    PRIMARY KEY (object_id, disk_id),
    INDEX replicas_by_disk (disk_id),
    FOREIGN KEY (object_id) REFERENCES objects (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

CREATE TABLE multipart_uploads (
    id VARCHAR(36) PRIMARY KEY,
    bucket VARCHAR(63) NOT NULL,
    object_key VARBINARY(1024) NOT NULL,
    content_type TEXT NULL,
    storage_class VARCHAR(64) NULL,
    user_metadata TEXT NOT NULL,
    desired_replicas BIGINT NOT NULL,
    replicas_explicit BOOLEAN NOT NULL,
    disks TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    INDEX multipart_uploads_by_key (bucket, object_key, id),
    FOREIGN KEY (bucket) REFERENCES buckets (name)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

CREATE TABLE multipart_parts (
    upload_id VARCHAR(36) NOT NULL,
    part_number BIGINT NOT NULL,
    blob_id VARCHAR(36) NOT NULL,
    size BIGINT NOT NULL,
    etag VARCHAR(64) NOT NULL,
    blake3 VARCHAR(64) NOT NULL,
    disks TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (upload_id, part_number),
    FOREIGN KEY (upload_id) REFERENCES multipart_uploads (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;
