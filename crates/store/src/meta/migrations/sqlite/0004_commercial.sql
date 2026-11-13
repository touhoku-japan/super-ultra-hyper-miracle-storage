ALTER TABLE tenants ADD COLUMN suspended BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE tenants ADD COLUMN quota_bytes INTEGER;
ALTER TABLE tenants ADD COLUMN max_buckets INTEGER;
ALTER TABLE buckets ADD COLUMN public_read BOOLEAN NOT NULL DEFAULT FALSE;

-- Billing meter: one row per tenant and hour (hour = unix ms of the hour's start).
CREATE TABLE usage_hourly (
    tenant_id TEXT NOT NULL,
    hour INTEGER NOT NULL,
    class_a_requests INTEGER NOT NULL DEFAULT 0,
    class_b_requests INTEGER NOT NULL DEFAULT 0,
    ingress_bytes INTEGER NOT NULL DEFAULT 0,
    egress_bytes INTEGER NOT NULL DEFAULT 0,
    stored_objects INTEGER NOT NULL DEFAULT 0,
    logical_bytes INTEGER NOT NULL DEFAULT 0,
    stored_bytes INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, hour)
);
CREATE INDEX multipart_uploads_by_age ON multipart_uploads (created_at);
