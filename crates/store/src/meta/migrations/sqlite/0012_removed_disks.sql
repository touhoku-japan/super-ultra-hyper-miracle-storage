-- Disks forgotten with `disk remove`. A node that still has one in its config refuses to
-- register it again (it would come back as an empty disk the cluster no longer counts on).
CREATE TABLE removed_disks (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL,
    -- Unix ms.
    removed_at INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE TRIGGER removed_disks_updated_at AFTER UPDATE ON removed_disks FOR EACH ROW
WHEN NEW.updated_at IS OLD.updated_at
BEGIN
    UPDATE removed_disks SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
