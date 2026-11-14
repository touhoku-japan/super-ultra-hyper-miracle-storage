-- Audit timestamps on every table: created_at (row inserted) and updated_at (row last changed).
-- The app's own unix-ms columns that were called created_at are renamed to what they mean
-- (objects/parts: last_modified, uploads: initiated_at, others: created_ms).

ALTER TABLE tenants RENAME COLUMN created_at TO created_ms;
ALTER TABLE access_keys RENAME COLUMN created_at TO created_ms;
ALTER TABLE buckets RENAME COLUMN created_at TO created_ms;
ALTER TABLE objects RENAME COLUMN created_at TO last_modified;
ALTER TABLE multipart_uploads RENAME COLUMN created_at TO initiated_at;
ALTER TABLE multipart_parts RENAME COLUMN created_at TO last_modified;

CREATE FUNCTION suhms_set_updated_at() RETURNS trigger AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END
$$ LANGUAGE plpgsql;

ALTER TABLE tenants ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
UPDATE tenants SET created_at = to_timestamp(created_ms / 1000.0), updated_at = to_timestamp(created_ms / 1000.0);
CREATE TRIGGER tenants_updated_at BEFORE UPDATE ON tenants
    FOR EACH ROW EXECUTE FUNCTION suhms_set_updated_at();

ALTER TABLE access_keys ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
UPDATE access_keys SET created_at = to_timestamp(created_ms / 1000.0), updated_at = to_timestamp(created_ms / 1000.0);
CREATE TRIGGER access_keys_updated_at BEFORE UPDATE ON access_keys
    FOR EACH ROW EXECUTE FUNCTION suhms_set_updated_at();

ALTER TABLE disks ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
CREATE TRIGGER disks_updated_at BEFORE UPDATE ON disks
    FOR EACH ROW EXECUTE FUNCTION suhms_set_updated_at();

ALTER TABLE buckets ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
UPDATE buckets SET created_at = to_timestamp(created_ms / 1000.0), updated_at = to_timestamp(created_ms / 1000.0);
CREATE TRIGGER buckets_updated_at BEFORE UPDATE ON buckets
    FOR EACH ROW EXECUTE FUNCTION suhms_set_updated_at();

ALTER TABLE objects ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
UPDATE objects SET created_at = to_timestamp(last_modified / 1000.0), updated_at = to_timestamp(last_modified / 1000.0);
CREATE TRIGGER objects_updated_at BEFORE UPDATE ON objects
    FOR EACH ROW EXECUTE FUNCTION suhms_set_updated_at();

ALTER TABLE replicas ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
CREATE TRIGGER replicas_updated_at BEFORE UPDATE ON replicas
    FOR EACH ROW EXECUTE FUNCTION suhms_set_updated_at();

ALTER TABLE multipart_uploads ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
UPDATE multipart_uploads SET created_at = to_timestamp(initiated_at / 1000.0), updated_at = to_timestamp(initiated_at / 1000.0);
CREATE TRIGGER multipart_uploads_updated_at BEFORE UPDATE ON multipart_uploads
    FOR EACH ROW EXECUTE FUNCTION suhms_set_updated_at();

ALTER TABLE multipart_parts ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
UPDATE multipart_parts SET created_at = to_timestamp(last_modified / 1000.0), updated_at = to_timestamp(last_modified / 1000.0);
CREATE TRIGGER multipart_parts_updated_at BEFORE UPDATE ON multipart_parts
    FOR EACH ROW EXECUTE FUNCTION suhms_set_updated_at();

ALTER TABLE usage_hourly ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
CREATE TRIGGER usage_hourly_updated_at BEFORE UPDATE ON usage_hourly
    FOR EACH ROW EXECUTE FUNCTION suhms_set_updated_at();

ALTER TABLE schema_migrations ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
CREATE TRIGGER schema_migrations_updated_at BEFORE UPDATE ON schema_migrations
    FOR EACH ROW EXECUTE FUNCTION suhms_set_updated_at();
