-- Operations: disk draining (decommission), a queue of objects that need repair (so repair
-- does not scan every object), and leases so only one repair/rebalance runs at a time.

ALTER TABLE disks ADD COLUMN draining BOOLEAN NOT NULL DEFAULT FALSE;
-- When repair noticed the disk was down (unix ms) and queued its objects; cleared (and the
-- objects queued again, to trim surplus copies) once it is back.
ALTER TABLE disks ADD COLUMN down_since INTEGER NULL;

-- Copies on one disk in object id order (draining, dead disks, GC, rebalance).
CREATE INDEX replicas_by_disk_object ON replicas (disk_id, object_id);
DROP INDEX replicas_by_disk;

CREATE TABLE repair_queue (
    object_id TEXT PRIMARY KEY REFERENCES objects (id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE TRIGGER repair_queue_updated_at AFTER UPDATE ON repair_queue FOR EACH ROW
WHEN NEW.updated_at IS OLD.updated_at
BEGIN
    UPDATE repair_queue SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;

CREATE TABLE leases (
    name TEXT PRIMARY KEY,
    holder TEXT NOT NULL,
    -- Unix ms.
    expires_at INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE TRIGGER leases_updated_at AFTER UPDATE ON leases FOR EACH ROW
WHEN NEW.updated_at IS OLD.updated_at
BEGIN
    UPDATE leases SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
