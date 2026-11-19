-- Files that could not be deleted when their object or upload went away, because the disk was
-- unreachable (its node down or cut off). The node owning the disk deletes them once it is back
-- (`Store::process_pending_removals`), instead of leaving them to gc. `kind` is `object` (an
-- object file, `target_id` = object id) or `upload` (the parts directory of an upload).
CREATE TABLE pending_removals (
    disk_id VARCHAR(36) NOT NULL,
    target_id VARCHAR(36) NOT NULL,
    kind VARCHAR(16) NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
    PRIMARY KEY (disk_id, target_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;
