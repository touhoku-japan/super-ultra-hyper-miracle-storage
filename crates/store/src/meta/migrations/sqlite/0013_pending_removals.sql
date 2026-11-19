-- Files that could not be deleted when their object or upload went away, because the disk was
-- unreachable (its node down or cut off). The node owning the disk deletes them once it is back
-- (`Store::process_pending_removals`), instead of leaving them to gc. `kind` is `object` (an
-- object file, `target_id` = object id) or `upload` (the parts directory of an upload).
CREATE TABLE pending_removals (
    disk_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (disk_id, target_id)
);
CREATE TRIGGER pending_removals_updated_at AFTER UPDATE ON pending_removals FOR EACH ROW
WHEN NEW.updated_at IS OLD.updated_at
BEGIN
    UPDATE pending_removals SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = NEW.rowid;
END;
