-- Operations: disk draining (decommission), a queue of objects that need repair (so repair
-- does not scan every object), and leases so only one repair/rebalance runs at a time.
-- (replicas_by_disk already orders by object id: InnoDB appends the primary key.)

ALTER TABLE disks ADD COLUMN draining BOOLEAN NOT NULL DEFAULT FALSE;
-- When repair noticed the disk was down (unix ms) and queued its objects; cleared (and the
-- objects queued again, to trim surplus copies) once it is back.
ALTER TABLE disks ADD COLUMN down_since BIGINT NULL;

CREATE TABLE repair_queue (
    object_id VARCHAR(36) PRIMARY KEY,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
    FOREIGN KEY (object_id) REFERENCES objects (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

CREATE TABLE leases (
    name VARCHAR(64) PRIMARY KEY,
    holder VARCHAR(255) NOT NULL,
    -- Unix ms.
    expires_at BIGINT NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;
