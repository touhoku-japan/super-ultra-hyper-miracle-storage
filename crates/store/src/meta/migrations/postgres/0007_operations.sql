-- Operations: disk draining (decommission), a queue of objects that need repair (so repair
-- does not scan every object), and leases so only one repair/rebalance runs at a time.

ALTER TABLE disks ADD COLUMN draining BOOLEAN NOT NULL DEFAULT FALSE;
-- When repair noticed the disk was down (unix ms) and queued its objects; cleared (and the
-- objects queued again, to trim surplus copies) once it is back.
ALTER TABLE disks ADD COLUMN down_since BIGINT NULL;

-- Copies on one disk in object id order (draining, dead disks, GC, rebalance). "C" collation so
-- GC can read the ids under one directory prefix as a range.
CREATE INDEX replicas_by_disk_object ON replicas (disk_id, object_id COLLATE "C");
DROP INDEX replicas_by_disk;

CREATE TABLE repair_queue (
    object_id TEXT PRIMARY KEY REFERENCES objects (id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TRIGGER repair_queue_updated_at BEFORE UPDATE ON repair_queue
    FOR EACH ROW EXECUTE FUNCTION suhms_set_updated_at();

CREATE TABLE leases (
    name TEXT PRIMARY KEY,
    holder TEXT NOT NULL,
    -- Unix ms.
    expires_at BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TRIGGER leases_updated_at BEFORE UPDATE ON leases
    FOR EACH ROW EXECUTE FUNCTION suhms_set_updated_at();
