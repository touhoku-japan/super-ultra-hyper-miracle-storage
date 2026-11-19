-- Disks forgotten with `disk remove`. A node that still has one in its config refuses to
-- register it again (it would come back as an empty disk the cluster no longer counts on).
CREATE TABLE removed_disks (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL,
    -- Unix ms.
    removed_at BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TRIGGER removed_disks_updated_at BEFORE UPDATE ON removed_disks
    FOR EACH ROW EXECUTE FUNCTION suhms_set_updated_at();
