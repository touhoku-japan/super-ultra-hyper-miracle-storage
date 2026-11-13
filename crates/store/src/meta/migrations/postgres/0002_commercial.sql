ALTER TABLE tenants ADD COLUMN suspended BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE tenants ADD COLUMN quota_bytes BIGINT;
ALTER TABLE tenants ADD COLUMN max_buckets BIGINT;
ALTER TABLE buckets ADD COLUMN public_read BOOLEAN NOT NULL DEFAULT FALSE;

-- Billing meter: one row per tenant and hour (hour = unix ms of the hour's start).
CREATE TABLE usage_hourly (
    tenant_id TEXT NOT NULL,
    hour BIGINT NOT NULL,
    class_a_requests BIGINT NOT NULL DEFAULT 0,
    class_b_requests BIGINT NOT NULL DEFAULT 0,
    ingress_bytes BIGINT NOT NULL DEFAULT 0,
    egress_bytes BIGINT NOT NULL DEFAULT 0,
    stored_objects BIGINT NOT NULL DEFAULT 0,
    logical_bytes BIGINT NOT NULL DEFAULT 0,
    stored_bytes BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, hour)
);
CREATE INDEX multipart_uploads_by_age ON multipart_uploads (created_at);
