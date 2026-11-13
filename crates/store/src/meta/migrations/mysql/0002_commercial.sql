ALTER TABLE tenants ADD COLUMN suspended BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE tenants ADD COLUMN quota_bytes BIGINT NULL;
ALTER TABLE tenants ADD COLUMN max_buckets BIGINT NULL;
ALTER TABLE buckets ADD COLUMN public_read BOOLEAN NOT NULL DEFAULT FALSE;

CREATE TABLE usage_hourly (
    tenant_id VARCHAR(36) NOT NULL,
    hour BIGINT NOT NULL,
    class_a_requests BIGINT NOT NULL DEFAULT 0,
    class_b_requests BIGINT NOT NULL DEFAULT 0,
    ingress_bytes BIGINT NOT NULL DEFAULT 0,
    egress_bytes BIGINT NOT NULL DEFAULT 0,
    stored_objects BIGINT NOT NULL DEFAULT 0,
    logical_bytes BIGINT NOT NULL DEFAULT 0,
    stored_bytes BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, hour)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

CREATE INDEX multipart_uploads_by_age ON multipart_uploads (created_at);
