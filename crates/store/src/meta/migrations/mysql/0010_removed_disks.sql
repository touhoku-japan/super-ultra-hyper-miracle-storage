-- Disks forgotten with `disk remove`. A node that still has one in its config refuses to
-- register it again (it would come back as an empty disk the cluster no longer counts on).
CREATE TABLE removed_disks (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    node_id VARCHAR(128) NOT NULL,
    -- Unix ms.
    removed_at BIGINT NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;
