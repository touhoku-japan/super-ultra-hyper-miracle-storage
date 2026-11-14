-- Audit timestamps on every table: created_at (row inserted) and updated_at (row last changed).
-- DATETIME(6) rather than TIMESTAMP (which ends in 2038); values are UTC because sqlx sets the
-- session time_zone to +00:00. The app's own unix-ms columns that were called created_at are
-- renamed to what they mean (objects/parts: last_modified, uploads: initiated_at, others: created_ms).

ALTER TABLE tenants RENAME COLUMN created_at TO created_ms;
ALTER TABLE access_keys RENAME COLUMN created_at TO created_ms;
ALTER TABLE buckets RENAME COLUMN created_at TO created_ms;
ALTER TABLE objects RENAME COLUMN created_at TO last_modified;
ALTER TABLE multipart_uploads RENAME COLUMN created_at TO initiated_at;
ALTER TABLE multipart_parts RENAME COLUMN created_at TO last_modified;

ALTER TABLE tenants ADD COLUMN created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    ADD COLUMN updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6);
UPDATE tenants SET created_at = FROM_UNIXTIME(created_ms / 1000), updated_at = FROM_UNIXTIME(created_ms / 1000);

ALTER TABLE access_keys ADD COLUMN created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    ADD COLUMN updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6);
UPDATE access_keys SET created_at = FROM_UNIXTIME(created_ms / 1000), updated_at = FROM_UNIXTIME(created_ms / 1000);

ALTER TABLE disks ADD COLUMN created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    ADD COLUMN updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6);

ALTER TABLE buckets ADD COLUMN created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    ADD COLUMN updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6);
UPDATE buckets SET created_at = FROM_UNIXTIME(created_ms / 1000), updated_at = FROM_UNIXTIME(created_ms / 1000);

ALTER TABLE objects ADD COLUMN created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    ADD COLUMN updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6);
UPDATE objects SET created_at = FROM_UNIXTIME(last_modified / 1000), updated_at = FROM_UNIXTIME(last_modified / 1000);

ALTER TABLE replicas ADD COLUMN created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    ADD COLUMN updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6);

ALTER TABLE multipart_uploads ADD COLUMN created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    ADD COLUMN updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6);
UPDATE multipart_uploads SET created_at = FROM_UNIXTIME(initiated_at / 1000), updated_at = FROM_UNIXTIME(initiated_at / 1000);

ALTER TABLE multipart_parts ADD COLUMN created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    ADD COLUMN updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6);
UPDATE multipart_parts SET created_at = FROM_UNIXTIME(last_modified / 1000), updated_at = FROM_UNIXTIME(last_modified / 1000);

ALTER TABLE usage_hourly ADD COLUMN created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    ADD COLUMN updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6);

ALTER TABLE schema_migrations ADD COLUMN created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    ADD COLUMN updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6);
