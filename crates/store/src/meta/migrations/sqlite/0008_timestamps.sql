-- Audit timestamps on every table: created_at (row inserted) and updated_at (row last changed),
-- as UTC ISO-8601 text ('2026-11-14T06:32:18.123Z'). SQLite cannot ADD COLUMN with a non-constant
-- default, so triggers fill them in. The app's own unix-ms columns that were called created_at are
-- renamed to what they mean (objects/parts: last_modified, uploads: initiated_at, others: created_ms).

ALTER TABLE tenants RENAME COLUMN created_at TO created_ms;
ALTER TABLE access_keys RENAME COLUMN created_at TO created_ms;
ALTER TABLE buckets RENAME COLUMN created_at TO created_ms;
ALTER TABLE objects RENAME COLUMN created_at TO last_modified;
ALTER TABLE multipart_uploads RENAME COLUMN created_at TO initiated_at;
ALTER TABLE multipart_parts RENAME COLUMN created_at TO last_modified;

ALTER TABLE tenants ADD COLUMN created_at TEXT;
ALTER TABLE tenants ADD COLUMN updated_at TEXT;
UPDATE tenants SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ', created_ms / 1000.0, 'unixepoch'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', created_ms / 1000.0, 'unixepoch');
CREATE TRIGGER tenants_created_at AFTER INSERT ON tenants FOR EACH ROW
BEGIN
    UPDATE tenants SET created_at = COALESCE(NEW.created_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
        updated_at = COALESCE(NEW.updated_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
    WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER tenants_updated_at AFTER UPDATE ON tenants FOR EACH ROW
WHEN NEW.updated_at IS OLD.updated_at
BEGIN
    UPDATE tenants SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;

ALTER TABLE access_keys ADD COLUMN created_at TEXT;
ALTER TABLE access_keys ADD COLUMN updated_at TEXT;
UPDATE access_keys SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ', created_ms / 1000.0, 'unixepoch'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', created_ms / 1000.0, 'unixepoch');
CREATE TRIGGER access_keys_created_at AFTER INSERT ON access_keys FOR EACH ROW
BEGIN
    UPDATE access_keys SET created_at = COALESCE(NEW.created_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
        updated_at = COALESCE(NEW.updated_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
    WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER access_keys_updated_at AFTER UPDATE ON access_keys FOR EACH ROW
WHEN NEW.updated_at IS OLD.updated_at
BEGIN
    UPDATE access_keys SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;

ALTER TABLE disks ADD COLUMN created_at TEXT;
ALTER TABLE disks ADD COLUMN updated_at TEXT;
UPDATE disks SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
CREATE TRIGGER disks_created_at AFTER INSERT ON disks FOR EACH ROW
BEGIN
    UPDATE disks SET created_at = COALESCE(NEW.created_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
        updated_at = COALESCE(NEW.updated_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
    WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER disks_updated_at AFTER UPDATE ON disks FOR EACH ROW
WHEN NEW.updated_at IS OLD.updated_at
BEGIN
    UPDATE disks SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;

ALTER TABLE buckets ADD COLUMN created_at TEXT;
ALTER TABLE buckets ADD COLUMN updated_at TEXT;
UPDATE buckets SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ', created_ms / 1000.0, 'unixepoch'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', created_ms / 1000.0, 'unixepoch');
CREATE TRIGGER buckets_created_at AFTER INSERT ON buckets FOR EACH ROW
BEGIN
    UPDATE buckets SET created_at = COALESCE(NEW.created_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
        updated_at = COALESCE(NEW.updated_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
    WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER buckets_updated_at AFTER UPDATE ON buckets FOR EACH ROW
WHEN NEW.updated_at IS OLD.updated_at
BEGIN
    UPDATE buckets SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;

ALTER TABLE objects ADD COLUMN created_at TEXT;
ALTER TABLE objects ADD COLUMN updated_at TEXT;
UPDATE objects SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ', last_modified / 1000.0, 'unixepoch'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', last_modified / 1000.0, 'unixepoch');
CREATE TRIGGER objects_created_at AFTER INSERT ON objects FOR EACH ROW
BEGIN
    UPDATE objects SET created_at = COALESCE(NEW.created_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
        updated_at = COALESCE(NEW.updated_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
    WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER objects_updated_at AFTER UPDATE ON objects FOR EACH ROW
WHEN NEW.updated_at IS OLD.updated_at
BEGIN
    UPDATE objects SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;

ALTER TABLE replicas ADD COLUMN created_at TEXT;
ALTER TABLE replicas ADD COLUMN updated_at TEXT;
UPDATE replicas SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
CREATE TRIGGER replicas_created_at AFTER INSERT ON replicas FOR EACH ROW
BEGIN
    UPDATE replicas SET created_at = COALESCE(NEW.created_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
        updated_at = COALESCE(NEW.updated_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
    WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER replicas_updated_at AFTER UPDATE ON replicas FOR EACH ROW
WHEN NEW.updated_at IS OLD.updated_at
BEGIN
    UPDATE replicas SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;

ALTER TABLE multipart_uploads ADD COLUMN created_at TEXT;
ALTER TABLE multipart_uploads ADD COLUMN updated_at TEXT;
UPDATE multipart_uploads SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ', initiated_at / 1000.0, 'unixepoch'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', initiated_at / 1000.0, 'unixepoch');
CREATE TRIGGER multipart_uploads_created_at AFTER INSERT ON multipart_uploads FOR EACH ROW
BEGIN
    UPDATE multipart_uploads SET created_at = COALESCE(NEW.created_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
        updated_at = COALESCE(NEW.updated_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
    WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER multipart_uploads_updated_at AFTER UPDATE ON multipart_uploads FOR EACH ROW
WHEN NEW.updated_at IS OLD.updated_at
BEGIN
    UPDATE multipart_uploads SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;

ALTER TABLE multipart_parts ADD COLUMN created_at TEXT;
ALTER TABLE multipart_parts ADD COLUMN updated_at TEXT;
UPDATE multipart_parts SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ', last_modified / 1000.0, 'unixepoch'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', last_modified / 1000.0, 'unixepoch');
CREATE TRIGGER multipart_parts_created_at AFTER INSERT ON multipart_parts FOR EACH ROW
BEGIN
    UPDATE multipart_parts SET created_at = COALESCE(NEW.created_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
        updated_at = COALESCE(NEW.updated_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
    WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER multipart_parts_updated_at AFTER UPDATE ON multipart_parts FOR EACH ROW
WHEN NEW.updated_at IS OLD.updated_at
BEGIN
    UPDATE multipart_parts SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;

ALTER TABLE usage_hourly ADD COLUMN created_at TEXT;
ALTER TABLE usage_hourly ADD COLUMN updated_at TEXT;
UPDATE usage_hourly SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
CREATE TRIGGER usage_hourly_created_at AFTER INSERT ON usage_hourly FOR EACH ROW
BEGIN
    UPDATE usage_hourly SET created_at = COALESCE(NEW.created_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
        updated_at = COALESCE(NEW.updated_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
    WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER usage_hourly_updated_at AFTER UPDATE ON usage_hourly FOR EACH ROW
WHEN NEW.updated_at IS OLD.updated_at
BEGIN
    UPDATE usage_hourly SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;

ALTER TABLE schema_migrations ADD COLUMN created_at TEXT;
ALTER TABLE schema_migrations ADD COLUMN updated_at TEXT;
UPDATE schema_migrations SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
CREATE TRIGGER schema_migrations_created_at AFTER INSERT ON schema_migrations FOR EACH ROW
BEGIN
    UPDATE schema_migrations SET created_at = COALESCE(NEW.created_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
        updated_at = COALESCE(NEW.updated_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
    WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER schema_migrations_updated_at AFTER UPDATE ON schema_migrations FOR EACH ROW
WHEN NEW.updated_at IS OLD.updated_at
BEGIN
    UPDATE schema_migrations SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
