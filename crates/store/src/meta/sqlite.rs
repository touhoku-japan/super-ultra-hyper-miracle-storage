use std::{str::FromStr, time::Duration};

use async_trait::async_trait;
use sqlx::{
    Row, SqliteConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteRow},
};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
    AccessKeyRecord, BucketRecord, BucketSetting, DeleteBucketOutcome, DiskRecord, MetadataStore,
    NULL_VERSION, ObjectRecord, PartRecord, ReplicaEntry, StorageUsage, StoredObject, TenantRecord,
    TenantUpdate, UploadRecord, UsageDelta, UsageRow, VersioningMode, decode_disks,
    decode_user_metadata, encode_disks, encode_user_metadata,
};
use crate::{
    disk::DiskId,
    error::{Result, StoreError},
};

/// Applied in order; each runs once, inside a transaction. Never edit a released migration.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("migrations/sqlite/0001_init.sql")),
    (2, include_str!("migrations/sqlite/0002_multipart.sql")),
    (3, include_str!("migrations/sqlite/0003_cluster.sql")),
    (4, include_str!("migrations/sqlite/0004_commercial.sql")),
    (5, include_str!("migrations/sqlite/0005_versioning.sql")),
];

macro_rules! object_select {
    ($where:literal) => {
        concat!(
            "SELECT id, bucket, object_key, size, etag, blake3, content_type, storage_class, ",
            "user_metadata, desired_replicas, replicas_explicit, created_at, version_id, ",
            "is_latest, delete_marker, noncurrent_at, lock_mode, lock_until, legal_hold ",
            "FROM objects WHERE ",
            $where
        )
    };
}

/// SQLite backend. Only for single-node deployments and development: the database file cannot be
/// shared between nodes.
pub struct SqliteMetadata {
    pool: SqlitePool,
    /// SQLite allows one writer; serializing write transactions here avoids lock-upgrade
    /// deadlocks (SQLITE_BUSY) between read-then-write transactions.
    write_lock: Mutex<()>,
}

impl SqliteMetadata {
    pub async fn connect(url: &str) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(10));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;
        migrate(&pool).await?;
        Ok(Self {
            pool,
            write_lock: Mutex::new(()),
        })
    }
}

async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::raw_sql("CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY)")
        .execute(pool)
        .await?;
    let current: Option<i64> = sqlx::query_scalar("SELECT MAX(version) FROM schema_migrations")
        .fetch_one(pool)
        .await?;
    for (version, sql) in MIGRATIONS {
        if Some(*version) <= current {
            continue;
        }
        // Table rebuilds must not cascade-delete dependent rows: run with foreign keys off
        // (the pragma is ignored inside a transaction, so set it on the connection first).
        let mut conn = pool.acquire().await?;
        sqlx::raw_sql("PRAGMA foreign_keys = OFF")
            .execute(&mut *conn)
            .await?;
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        sqlx::raw_sql(*sql).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO schema_migrations (version) VALUES (?)")
            .bind(version)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        sqlx::raw_sql("PRAGMA foreign_keys = ON")
            .execute(&mut *conn)
            .await?;
        tracing::info!(version, "applied metadata migration");
    }
    Ok(())
}

fn u64_of(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn parse_uuid(s: &str) -> Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| StoreError::Metadata(Box::new(e)))
}

fn to_u8(v: i64) -> Result<u8> {
    u8::try_from(v).map_err(|e| StoreError::Metadata(Box::new(e)))
}

fn object_from_row(row: &SqliteRow) -> Result<ObjectRecord> {
    Ok(ObjectRecord {
        id: parse_uuid(row.try_get("id")?)?,
        bucket: row.try_get("bucket")?,
        key: row.try_get("object_key")?,
        size: u64::try_from(row.try_get::<i64, _>("size")?)
            .map_err(|e| StoreError::Metadata(Box::new(e)))?,
        etag: row.try_get("etag")?,
        blake3: row.try_get("blake3")?,
        content_type: row.try_get("content_type")?,
        storage_class: row.try_get("storage_class")?,
        user_metadata: decode_user_metadata(row.try_get("user_metadata")?)?,
        desired_replicas: to_u8(row.try_get("desired_replicas")?)?,
        replicas_explicit: row.try_get("replicas_explicit")?,
        created_at: row.try_get("created_at")?,
        version_id: row.try_get("version_id")?,
        is_latest: row.try_get("is_latest")?,
        delete_marker: row.try_get("delete_marker")?,
        noncurrent_at: row.try_get("noncurrent_at")?,
        lock_mode: row.try_get("lock_mode")?,
        lock_until: row.try_get("lock_until")?,
        legal_hold: row.try_get("legal_hold")?,
    })
}

fn bucket_from_row(r: &SqliteRow) -> Result<BucketRecord> {
    Ok(BucketRecord {
        name: r.try_get("name")?,
        owner: r.try_get("owner")?,
        replicas: r
            .try_get::<Option<i64>, _>("replicas")?
            .map(to_u8)
            .transpose()?,
        created_at: r.try_get("created_at")?,
        public_read: r.try_get("public_read")?,
        versioning: r.try_get("versioning")?,
        lifecycle: r.try_get("lifecycle")?,
        cors: r.try_get("cors")?,
        object_lock: r.try_get("object_lock")?,
    })
}

macro_rules! upload_select {
    ($where:literal) => {
        concat!(
            "SELECT id, bucket, object_key, content_type, storage_class, user_metadata, ",
            "desired_replicas, replicas_explicit, disks, created_at FROM multipart_uploads WHERE ",
            $where
        )
    };
}

macro_rules! part_select {
    ($where:literal) => {
        concat!(
            "SELECT upload_id, part_number, blob_id, size, etag, blake3, disks, created_at ",
            "FROM multipart_parts WHERE ",
            $where
        )
    };
}

fn upload_from_row(r: &SqliteRow) -> Result<UploadRecord> {
    Ok(UploadRecord {
        id: parse_uuid(r.try_get("id")?)?,
        bucket: r.try_get("bucket")?,
        key: r.try_get("object_key")?,
        content_type: r.try_get("content_type")?,
        storage_class: r.try_get("storage_class")?,
        user_metadata: decode_user_metadata(r.try_get("user_metadata")?)?,
        desired_replicas: to_u8(r.try_get("desired_replicas")?)?,
        replicas_explicit: r.try_get("replicas_explicit")?,
        disks: decode_disks(r.try_get("disks")?)?,
        created_at: r.try_get("created_at")?,
    })
}

fn part_from_row(r: &SqliteRow) -> Result<PartRecord> {
    Ok(PartRecord {
        upload_id: parse_uuid(r.try_get("upload_id")?)?,
        part_number: u32::try_from(r.try_get::<i64, _>("part_number")?)
            .map_err(|e| StoreError::Metadata(Box::new(e)))?,
        blob_id: parse_uuid(r.try_get("blob_id")?)?,
        size: u64::try_from(r.try_get::<i64, _>("size")?)
            .map_err(|e| StoreError::Metadata(Box::new(e)))?,
        etag: r.try_get("etag")?,
        blake3: r.try_get("blake3")?,
        disks: decode_disks(r.try_get("disks")?)?,
        created_at: r.try_get("created_at")?,
    })
}

async fn load_parts(conn: &mut SqliteConnection, upload_id: Uuid) -> Result<Vec<PartRecord>> {
    sqlx::query(part_select!("upload_id = ? ORDER BY part_number"))
        .bind(upload_id.to_string())
        .fetch_all(&mut *conn)
        .await?
        .iter()
        .map(part_from_row)
        .collect()
}

fn i64_of(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|e| StoreError::Metadata(Box::new(e)))
}

/// Inserts an object row and its replica rows (inside the caller's transaction).
async fn insert_object(
    conn: &mut SqliteConnection,
    object: &ObjectRecord,
    replicas: &[DiskId],
) -> Result<()> {
    sqlx::query(
        "INSERT INTO objects (id, bucket, object_key, size, etag, blake3, content_type, \
         storage_class, user_metadata, desired_replicas, replicas_explicit, created_at, \
         version_id, is_latest, delete_marker, lock_mode, lock_until, legal_hold) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, TRUE, ?, ?, ?, ?)",
    )
    .bind(object.id.to_string())
    .bind(&object.bucket)
    .bind(&object.key)
    .bind(i64_of(object.size)?)
    .bind(&object.etag)
    .bind(&object.blake3)
    .bind(&object.content_type)
    .bind(&object.storage_class)
    .bind(encode_user_metadata(&object.user_metadata))
    .bind(i64::from(object.desired_replicas))
    .bind(object.replicas_explicit)
    .bind(object.created_at)
    .bind(&object.version_id)
    .bind(object.delete_marker)
    .bind(&object.lock_mode)
    .bind(object.lock_until)
    .bind(object.legal_hold)
    .execute(&mut *conn)
    .await?;
    for disk in replicas {
        sqlx::query("INSERT INTO replicas (object_id, disk_id) VALUES (?, ?)")
            .bind(object.id.to_string())
            .bind(disk.to_string())
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Removes the object at (bucket, key), if any, and returns it (inside the caller's transaction).
async fn take_object(
    conn: &mut SqliteConnection,
    bucket: &str,
    key: &str,
) -> Result<Option<StoredObject>> {
    let previous = load_by_key(conn, bucket, key).await?;
    if let Some(prev) = &previous {
        sqlx::query("DELETE FROM objects WHERE id = ?")
            .bind(prev.object.id.to_string())
            .execute(&mut *conn)
            .await?;
    }
    Ok(previous)
}

async fn demote(conn: &mut SqliteConnection, id: Uuid, now: i64) -> Result<()> {
    sqlx::query("UPDATE objects SET is_latest = FALSE, noncurrent_at = ? WHERE id = ?")
        .bind(now)
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn load_version(
    conn: &mut SqliteConnection,
    bucket: &str,
    key: &str,
    version_id: &str,
) -> Result<Option<StoredObject>> {
    let row = sqlx::query(object_select!(
        "bucket = ? AND object_key = ? AND version_id = ?"
    ))
    .bind(bucket)
    .bind(key)
    .bind(version_id)
    .fetch_optional(&mut *conn)
    .await?;
    finish(conn, row).await
}

async fn delete_row(conn: &mut SqliteConnection, id: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM objects WHERE id = ?")
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Makes room for `object` as the key's new latest version and inserts it (inside the
/// caller's transaction). Returns the versions that were removed.
async fn place_version(
    conn: &mut SqliteConnection,
    object: &ObjectRecord,
    replicas: &[DiskId],
    if_absent: bool,
    mode: VersioningMode,
) -> Result<Vec<StoredObject>> {
    let latest = load_by_key(conn, &object.bucket, &object.key).await?;
    if if_absent && latest.as_ref().is_some_and(|l| !l.object.delete_marker) {
        return Err(StoreError::PreconditionFailed);
    }
    let mut removed = Vec::new();
    match mode {
        VersioningMode::Unversioned => {
            if let Some(l) = latest {
                delete_row(conn, l.object.id).await?;
                removed.push(l);
            }
        }
        VersioningMode::Enabled => {
            if let Some(l) = latest {
                demote(conn, l.object.id, object.created_at).await?;
            }
        }
        VersioningMode::Suspended => {
            if let Some(n) = load_version(conn, &object.bucket, &object.key, NULL_VERSION).await? {
                delete_row(conn, n.object.id).await?;
                removed.push(n);
            }
            if let Some(l) = latest
                && l.object.version_id != NULL_VERSION
            {
                demote(conn, l.object.id, object.created_at).await?;
            }
        }
    }
    insert_object(conn, object, replicas).await?;
    Ok(removed)
}

fn tenant_from_row(r: &SqliteRow) -> Result<TenantRecord> {
    Ok(TenantRecord {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        created_at: r.try_get("created_at")?,
        suspended: r.try_get("suspended")?,
        quota_bytes: r.try_get::<Option<i64>, _>("quota_bytes")?.map(u64_of),
        max_buckets: r.try_get::<Option<i64>, _>("max_buckets")?.map(u64_of),
    })
}

fn key_from_row(r: &SqliteRow) -> Result<AccessKeyRecord> {
    Ok(AccessKeyRecord {
        id: r.try_get("id")?,
        tenant_id: r.try_get("tenant_id")?,
        secret_enc: r.try_get("secret_enc")?,
        active: r.try_get("active")?,
        created_at: r.try_get("created_at")?,
    })
}

async fn load_replicas(conn: &mut SqliteConnection, id: Uuid) -> Result<Vec<DiskId>> {
    let rows = sqlx::query("SELECT disk_id FROM replicas WHERE object_id = ? ORDER BY disk_id")
        .bind(id.to_string())
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(|r| parse_uuid(r.try_get("disk_id")?))
        .collect()
}

async fn finish(
    conn: &mut SqliteConnection,
    row: Option<SqliteRow>,
) -> Result<Option<StoredObject>> {
    let Some(row) = row else {
        return Ok(None);
    };
    let object = object_from_row(&row)?;
    let replicas = load_replicas(conn, object.id).await?;
    Ok(Some(StoredObject { object, replicas }))
}

async fn load_by_key(
    conn: &mut SqliteConnection,
    bucket: &str,
    key: &str,
) -> Result<Option<StoredObject>> {
    let row = sqlx::query(object_select!(
        "bucket = ? AND object_key = ? AND is_latest = TRUE"
    ))
    .bind(bucket)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    finish(conn, row).await
}

async fn load_by_id(conn: &mut SqliteConnection, id: Uuid) -> Result<Option<StoredObject>> {
    let row = sqlx::query(object_select!("id = ?"))
        .bind(id.to_string())
        .fetch_optional(&mut *conn)
        .await?;
    finish(conn, row).await
}

#[async_trait]
impl MetadataStore for SqliteMetadata {
    async fn update_tenant(&self, id: &str, update: &TenantUpdate) -> Result<bool> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await?;
        let exists = sqlx::query("SELECT 1 FROM tenants WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
        if !exists {
            return Ok(false);
        }
        if let Some(v) = update.suspended {
            sqlx::query("UPDATE tenants SET suspended = ? WHERE id = ?")
                .bind(v)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(v) = update.quota_bytes {
            sqlx::query("UPDATE tenants SET quota_bytes = ? WHERE id = ?")
                .bind(v.map(i64_of).transpose()?)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(v) = update.max_buckets {
            sqlx::query("UPDATE tenants SET max_buckets = ? WHERE id = ?")
                .bind(v.map(i64_of).transpose()?)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    async fn set_bucket_public(&self, name: &str, public_read: bool) -> Result<bool> {
        let _guard = self.write_lock.lock().await;
        let res = sqlx::query("UPDATE buckets SET public_read = ? WHERE name = ?")
            .bind(public_read)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() == 1)
    }

    async fn list_stale_uploads(
        &self,
        created_before: i64,
        limit: u32,
    ) -> Result<Vec<UploadRecord>> {
        sqlx::query(upload_select!("created_at < ? ORDER BY created_at LIMIT ?"))
            .bind(created_before)
            .bind(i64::from(limit))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(upload_from_row)
            .collect()
    }

    async fn storage_usage(&self, tenant_id: Option<&str>) -> Result<Vec<StorageUsage>> {
        let rows = sqlx::query(
            "SELECT b.owner AS tenant_id, COUNT(*) AS objects, SUM(o.size) AS logical_bytes, \
             SUM(o.size * o.desired_replicas) AS stored_bytes \
             FROM objects o JOIN buckets b ON b.name = o.bucket \
             WHERE ? IS NULL OR b.owner = ? GROUP BY b.owner ORDER BY b.owner",
        )
        .bind(tenant_id)
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(StorageUsage {
                    tenant_id: r.try_get("tenant_id")?,
                    objects: u64_of(r.try_get::<i64, _>("objects")?),
                    logical_bytes: u64_of(r.try_get::<i64, _>("logical_bytes")?),
                    stored_bytes: u64_of(r.try_get::<i64, _>("stored_bytes")?),
                })
            })
            .collect()
    }

    async fn add_usage(&self, deltas: &[UsageDelta]) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await?;
        for d in deltas {
            sqlx::query(
                "INSERT INTO usage_hourly (tenant_id, hour, class_a_requests, class_b_requests, \
     ingress_bytes, egress_bytes) VALUES (?, ?, ?, ?, ?, ?) \
     ON CONFLICT (tenant_id, hour) DO UPDATE SET \
     class_a_requests = usage_hourly.class_a_requests + excluded.class_a_requests, \
     class_b_requests = usage_hourly.class_b_requests + excluded.class_b_requests, \
     ingress_bytes = usage_hourly.ingress_bytes + excluded.ingress_bytes, \
     egress_bytes = usage_hourly.egress_bytes + excluded.egress_bytes",
            )
            .bind(&d.tenant_id)
            .bind(d.hour)
            .bind(i64_of(d.class_a)?)
            .bind(i64_of(d.class_b)?)
            .bind(i64_of(d.ingress_bytes)?)
            .bind(i64_of(d.egress_bytes)?)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn record_storage(&self, hour: i64, usage: &[StorageUsage]) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await?;
        for u in usage {
            sqlx::query("INSERT INTO usage_hourly (tenant_id, hour, stored_objects, logical_bytes, stored_bytes) \
     VALUES (?, ?, ?, ?, ?) ON CONFLICT (tenant_id, hour) DO UPDATE SET \
     stored_objects = excluded.stored_objects, logical_bytes = excluded.logical_bytes, \
     stored_bytes = excluded.stored_bytes")
                .bind(&u.tenant_id)
                .bind(hour)
                .bind(i64_of(u.objects)?)
                .bind(i64_of(u.logical_bytes)?)
                .bind(i64_of(u.stored_bytes)?)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn usage(&self, tenant_id: &str, from: i64, to: i64) -> Result<Vec<UsageRow>> {
        sqlx::query(
            "SELECT hour, class_a_requests, class_b_requests, ingress_bytes, egress_bytes, \
             stored_objects, logical_bytes, stored_bytes FROM usage_hourly \
             WHERE tenant_id = ? AND hour >= ? AND hour < ? ORDER BY hour",
        )
        .bind(tenant_id)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| {
            Ok(UsageRow {
                hour: r.try_get("hour")?,
                class_a_requests: u64_of(r.try_get("class_a_requests")?),
                class_b_requests: u64_of(r.try_get("class_b_requests")?),
                ingress_bytes: u64_of(r.try_get("ingress_bytes")?),
                egress_bytes: u64_of(r.try_get("egress_bytes")?),
                stored_objects: u64_of(r.try_get("stored_objects")?),
                logical_bytes: u64_of(r.try_get("logical_bytes")?),
                stored_bytes: u64_of(r.try_get("stored_bytes")?),
            })
        })
        .collect()
    }

    async fn get_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Option<StoredObject>> {
        let mut conn = self.pool.acquire().await?;
        load_version(&mut conn, bucket, key, version_id).await
    }

    async fn delete_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Option<StoredObject>> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await?;
        let Some(v) = load_version(&mut tx, bucket, key, version_id).await? else {
            return Ok(None);
        };
        delete_row(&mut tx, v.object.id).await?;
        if v.object.is_latest {
            let next = sqlx::query(object_select!(
                "bucket = ? AND object_key = ? ORDER BY created_at DESC, id DESC LIMIT 1"
            ))
            .bind(bucket)
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(row) = next {
                let next = object_from_row(&row)?;
                sqlx::query(
                    "UPDATE objects SET is_latest = TRUE, noncurrent_at = NULL WHERE id = ?",
                )
                .bind(next.id.to_string())
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(Some(v))
    }

    async fn list_object_versions(
        &self,
        bucket: &str,
        prefix: &str,
        after: (&str, i64, &str),
        limit: u32,
    ) -> Result<Vec<ObjectRecord>> {
        let (key, created, id) = after;
        sqlx::query(object_select!(
            "bucket = ? AND object_key >= ? AND (object_key > ? OR (object_key = ? AND \
             (created_at < ? OR (created_at = ? AND id < ?)))) \
             ORDER BY object_key, created_at DESC, id DESC LIMIT ?"
        ))
        .bind(bucket)
        .bind(prefix)
        .bind(key)
        .bind(key)
        .bind(created)
        .bind(created)
        .bind(id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(object_from_row)
        .collect()
    }

    async fn lifecycle_candidates(
        &self,
        bucket: &str,
        prefix: &str,
        noncurrent: bool,
        before: i64,
        after: (&str, &str),
        limit: u32,
    ) -> Result<Vec<ObjectRecord>> {
        let sql = if noncurrent {
            object_select!(
                "bucket = ? AND object_key >= ? AND (object_key, id) > (?, ?) \
                 AND is_latest = FALSE AND noncurrent_at < ? ORDER BY object_key, id LIMIT ?"
            )
        } else {
            object_select!(
                "bucket = ? AND object_key >= ? AND (object_key, id) > (?, ?) \
                 AND is_latest = TRUE AND delete_marker = FALSE AND created_at < ? \
                 ORDER BY object_key, id LIMIT ?"
            )
        };
        sqlx::query(sql)
            .bind(bucket)
            .bind(prefix)
            .bind(after.0)
            .bind(after.1)
            .bind(before)
            .bind(i64::from(limit))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(object_from_row)
            .collect()
    }

    async fn set_object_lock(
        &self,
        object_id: Uuid,
        mode: Option<&str>,
        until: Option<i64>,
    ) -> Result<bool> {
        let _guard = self.write_lock.lock().await;
        let res = sqlx::query("UPDATE objects SET lock_mode = ?, lock_until = ? WHERE id = ?")
            .bind(mode)
            .bind(until)
            .bind(object_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() == 1)
    }

    async fn set_legal_hold(&self, object_id: Uuid, on: bool) -> Result<bool> {
        let _guard = self.write_lock.lock().await;
        let res = sqlx::query("UPDATE objects SET legal_hold = ? WHERE id = ?")
            .bind(on)
            .bind(object_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() == 1)
    }

    async fn set_bucket_setting(
        &self,
        name: &str,
        setting: BucketSetting,
        value: Option<&str>,
    ) -> Result<bool> {
        let _guard = self.write_lock.lock().await;
        let sql = match setting {
            BucketSetting::Versioning => "UPDATE buckets SET versioning = ? WHERE name = ?",
            BucketSetting::Lifecycle => "UPDATE buckets SET lifecycle = ? WHERE name = ?",
            BucketSetting::Cors => "UPDATE buckets SET cors = ? WHERE name = ?",
            BucketSetting::ObjectLock => "UPDATE buckets SET object_lock = ? WHERE name = ?",
        };
        let exists = sqlx::query("SELECT 1 FROM buckets WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .is_some();
        if !exists {
            return Ok(false);
        }
        sqlx::query(sql)
            .bind(value)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(true)
    }

    async fn list_all_buckets(&self) -> Result<Vec<BucketRecord>> {
        sqlx::query(
            "SELECT name, owner, replicas, created_at, public_read, versioning, lifecycle, cors, \
             object_lock FROM buckets ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(bucket_from_row)
        .collect()
    }

    async fn create_tenant(&self, tenant: &TenantRecord) -> Result<bool> {
        let _guard = self.write_lock.lock().await;
        let res = sqlx::query(
            "INSERT INTO tenants (id, name, created_at) VALUES (?, ?, ?) \
             ON CONFLICT DO NOTHING",
        )
        .bind(&tenant.id)
        .bind(&tenant.name)
        .bind(tenant.created_at)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    async fn get_tenant(&self, id: &str) -> Result<Option<TenantRecord>> {
        sqlx::query("SELECT id, name, created_at, suspended, quota_bytes, max_buckets FROM tenants WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(tenant_from_row)
            .transpose()
    }

    async fn get_tenant_by_name(&self, name: &str) -> Result<Option<TenantRecord>> {
        sqlx::query("SELECT id, name, created_at, suspended, quota_bytes, max_buckets FROM tenants WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(tenant_from_row)
            .transpose()
    }

    async fn list_tenants(&self) -> Result<Vec<TenantRecord>> {
        sqlx::query("SELECT id, name, created_at, suspended, quota_bytes, max_buckets FROM tenants ORDER BY name")
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(tenant_from_row)
            .collect()
    }

    async fn create_access_key(&self, key: &AccessKeyRecord) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        sqlx::query(
            "INSERT INTO access_keys (id, tenant_id, secret_enc, active, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&key.id)
        .bind(&key.tenant_id)
        .bind(&key.secret_enc)
        .bind(key.active)
        .bind(key.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_access_key(&self, id: &str) -> Result<Option<AccessKeyRecord>> {
        sqlx::query(
            "SELECT id, tenant_id, secret_enc, active, created_at FROM access_keys WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(key_from_row)
        .transpose()
    }

    async fn list_access_keys(&self, tenant_id: &str) -> Result<Vec<AccessKeyRecord>> {
        sqlx::query(
            "SELECT id, tenant_id, secret_enc, active, created_at FROM access_keys \
             WHERE tenant_id = ? ORDER BY created_at, id",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(key_from_row)
        .collect()
    }

    async fn set_access_key_active(&self, id: &str, active: bool) -> Result<bool> {
        let _guard = self.write_lock.lock().await;
        let res = sqlx::query("UPDATE access_keys SET active = ? WHERE id = ?")
            .bind(active)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() == 1)
    }

    async fn set_node_disks_offline(&self, node_id: &str) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        sqlx::query("UPDATE disks SET online = FALSE WHERE node_id = ?")
            .bind(node_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn register_disk(
        &self,
        disk_id: DiskId,
        node_id: &str,
        path: &str,
        free_bytes: u64,
        now: i64,
    ) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        sqlx::query(
            "INSERT INTO disks (id, node_id, path, online, free_bytes, heartbeat_at) \
             VALUES (?, ?, ?, TRUE, ?, ?) \
             ON CONFLICT (id) DO UPDATE SET node_id = excluded.node_id, path = excluded.path, \
             online = TRUE, free_bytes = excluded.free_bytes, heartbeat_at = excluded.heartbeat_at",
        )
        .bind(disk_id.to_string())
        .bind(node_id)
        .bind(path)
        .bind(i64_of(free_bytes)?)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn heartbeat_disk(&self, disk_id: DiskId, free_bytes: u64, now: i64) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        sqlx::query("UPDATE disks SET free_bytes = ?, heartbeat_at = ? WHERE id = ?")
            .bind(i64_of(free_bytes)?)
            .bind(now)
            .bind(disk_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_disks(&self) -> Result<Vec<DiskRecord>> {
        sqlx::query(
            "SELECT id, node_id, path, online, free_bytes, heartbeat_at FROM disks \
             ORDER BY node_id, path",
        )
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| {
            Ok(DiskRecord {
                id: parse_uuid(r.try_get("id")?)?,
                node_id: r.try_get("node_id")?,
                path: r.try_get("path")?,
                online: r.try_get("online")?,
                free_bytes: u64::try_from(r.try_get::<i64, _>("free_bytes")?).unwrap_or(0),
                heartbeat_at: r.try_get("heartbeat_at")?,
            })
        })
        .collect()
    }

    async fn create_bucket(&self, bucket: &BucketRecord) -> Result<bool> {
        let _guard = self.write_lock.lock().await;
        let res = sqlx::query(
            "INSERT INTO buckets (name, owner, replicas, created_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(&bucket.name)
        .bind(&bucket.owner)
        .bind(bucket.replicas.map(i64::from))
        .bind(bucket.created_at)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    async fn get_bucket(&self, name: &str) -> Result<Option<BucketRecord>> {
        sqlx::query(
            "SELECT name, owner, replicas, created_at, public_read, versioning, lifecycle, cors, object_lock FROM buckets WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(bucket_from_row)
        .transpose()
    }

    async fn list_buckets(&self, owner: &str) -> Result<Vec<BucketRecord>> {
        sqlx::query(
            "SELECT name, owner, replicas, created_at, public_read, versioning, lifecycle, cors, object_lock FROM buckets WHERE owner = ? ORDER BY name",
        )
        .bind(owner)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(bucket_from_row)
        .collect()
    }

    async fn set_bucket_replicas(
        &self,
        name: &str,
        replicas: Option<u8>,
        effective: u8,
    ) -> Result<bool> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await?;
        let res = sqlx::query("UPDATE buckets SET replicas = ? WHERE name = ?")
            .bind(replicas.map(i64::from))
            .bind(name)
            .execute(&mut *tx)
            .await?;
        if res.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query(
            "UPDATE objects SET desired_replicas = ? WHERE bucket = ? AND replicas_explicit = FALSE",
        )
        .bind(i64::from(effective))
        .bind(name)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn delete_bucket(&self, name: &str) -> Result<DeleteBucketOutcome> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await?;
        let has_objects = sqlx::query("SELECT 1 FROM objects WHERE bucket = ? LIMIT 1")
            .bind(name)
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
        if has_objects {
            return Ok(DeleteBucketOutcome::NotEmpty);
        }
        let res = sqlx::query("DELETE FROM buckets WHERE name = ?")
            .bind(name)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(if res.rows_affected() == 1 {
            DeleteBucketOutcome::Deleted
        } else {
            DeleteBucketOutcome::NotFound
        })
    }

    async fn put_object(
        &self,
        object: &ObjectRecord,
        replicas: &[DiskId],
        if_absent: bool,
        mode: VersioningMode,
    ) -> Result<Vec<StoredObject>> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await?;
        let bucket_exists = sqlx::query("SELECT 1 FROM buckets WHERE name = ?")
            .bind(&object.bucket)
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
        if !bucket_exists {
            return Err(StoreError::NoSuchBucket(object.bucket.clone()));
        }
        let removed = place_version(&mut tx, object, replicas, if_absent, mode).await?;
        tx.commit().await?;
        Ok(removed)
    }

    async fn get_object(&self, bucket: &str, key: &str) -> Result<Option<StoredObject>> {
        let mut conn = self.pool.acquire().await?;
        load_by_key(&mut conn, bucket, key).await
    }

    async fn get_object_by_id(&self, id: Uuid) -> Result<Option<StoredObject>> {
        let mut conn = self.pool.acquire().await?;
        load_by_id(&mut conn, id).await
    }

    async fn delete_object(&self, bucket: &str, key: &str) -> Result<Option<StoredObject>> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await?;
        let previous = take_object(&mut tx, bucket, key).await?;
        tx.commit().await?;
        Ok(previous)
    }

    async fn list_objects(
        &self,
        bucket: &str,
        from: &str,
        after: &str,
        limit: u32,
    ) -> Result<Vec<ObjectRecord>> {
        // SQLite's default BINARY collation compares UTF-8 bytes, which is S3's key order.
        sqlx::query(object_select!(
            "bucket = ? AND object_key >= ? AND object_key > ? AND is_latest = TRUE \
             AND delete_marker = FALSE ORDER BY object_key LIMIT ?"
        ))
        .bind(bucket)
        .bind(from)
        .bind(after)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(object_from_row)
        .collect()
    }

    async fn create_upload(&self, upload: &UploadRecord) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await?;
        let bucket_exists = sqlx::query("SELECT 1 FROM buckets WHERE name = ?")
            .bind(&upload.bucket)
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
        if !bucket_exists {
            return Err(StoreError::NoSuchBucket(upload.bucket.clone()));
        }
        sqlx::query(
            "INSERT INTO multipart_uploads (id, bucket, object_key, content_type, storage_class, \
             user_metadata, desired_replicas, replicas_explicit, disks, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(upload.id.to_string())
        .bind(&upload.bucket)
        .bind(&upload.key)
        .bind(&upload.content_type)
        .bind(&upload.storage_class)
        .bind(encode_user_metadata(&upload.user_metadata))
        .bind(i64::from(upload.desired_replicas))
        .bind(upload.replicas_explicit)
        .bind(encode_disks(&upload.disks))
        .bind(upload.created_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn get_upload(&self, id: Uuid) -> Result<Option<UploadRecord>> {
        sqlx::query(upload_select!("id = ?"))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(upload_from_row)
            .transpose()
    }

    async fn list_uploads(
        &self,
        bucket: &str,
        prefix: &str,
        after_key: &str,
        after_id: &str,
        limit: u32,
    ) -> Result<Vec<UploadRecord>> {
        sqlx::query(upload_select!(
            "bucket = ? AND object_key >= ? AND (object_key, id) > (?, ?) \
             ORDER BY object_key, id LIMIT ?"
        ))
        .bind(bucket)
        .bind(prefix)
        .bind(after_key)
        .bind(after_id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(upload_from_row)
        .filter(|u| u.as_ref().map_or(true, |u| u.key.starts_with(prefix)))
        .collect()
    }

    async fn put_part(&self, part: &PartRecord) -> Result<Option<PartRecord>> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await?;
        let upload_exists = sqlx::query("SELECT 1 FROM multipart_uploads WHERE id = ?")
            .bind(part.upload_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
        if !upload_exists {
            return Err(StoreError::NoSuchUpload);
        }
        let previous = sqlx::query(part_select!("upload_id = ? AND part_number = ?"))
            .bind(part.upload_id.to_string())
            .bind(i64::from(part.part_number))
            .fetch_optional(&mut *tx)
            .await?
            .as_ref()
            .map(part_from_row)
            .transpose()?;
        sqlx::query(
            "INSERT INTO multipart_parts (upload_id, part_number, blob_id, size, etag, blake3, \
             disks, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (upload_id, part_number) DO UPDATE SET blob_id = excluded.blob_id, \
             size = excluded.size, etag = excluded.etag, blake3 = excluded.blake3, \
             disks = excluded.disks, created_at = excluded.created_at",
        )
        .bind(part.upload_id.to_string())
        .bind(i64::from(part.part_number))
        .bind(part.blob_id.to_string())
        .bind(i64_of(part.size)?)
        .bind(&part.etag)
        .bind(&part.blake3)
        .bind(encode_disks(&part.disks))
        .bind(part.created_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(previous)
    }

    async fn list_parts(
        &self,
        upload_id: Uuid,
        after_part: u32,
        limit: u32,
    ) -> Result<Vec<PartRecord>> {
        sqlx::query(part_select!(
            "upload_id = ? AND part_number > ? ORDER BY part_number LIMIT ?"
        ))
        .bind(upload_id.to_string())
        .bind(i64::from(after_part))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(part_from_row)
        .collect()
    }

    async fn delete_upload(&self, id: Uuid) -> Result<Option<(UploadRecord, Vec<PartRecord>)>> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await?;
        let Some(upload) = sqlx::query(upload_select!("id = ?"))
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .as_ref()
            .map(upload_from_row)
            .transpose()?
        else {
            return Ok(None);
        };
        let parts = load_parts(&mut tx, id).await?;
        sqlx::query("DELETE FROM multipart_uploads WHERE id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some((upload, parts)))
    }

    async fn complete_upload(
        &self,
        upload_id: Uuid,
        object: &ObjectRecord,
        replicas: &[DiskId],
        mode: VersioningMode,
    ) -> Result<(Vec<PartRecord>, Vec<StoredObject>)> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await?;
        let parts = load_parts(&mut tx, upload_id).await?;
        let res = sqlx::query("DELETE FROM multipart_uploads WHERE id = ?")
            .bind(upload_id.to_string())
            .execute(&mut *tx)
            .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NoSuchUpload);
        }
        let removed = place_version(&mut tx, object, replicas, false, mode).await?;
        tx.commit().await?;
        Ok((parts, removed))
    }

    async fn add_replica(&self, object_id: Uuid, disk_id: DiskId) -> Result<bool> {
        let _guard = self.write_lock.lock().await;
        // INSERT ... SELECT so a concurrently deleted object yields 0 rows instead of an
        // orphaned replica row.
        let res = sqlx::query(
            "INSERT INTO replicas (object_id, disk_id) SELECT id, ? FROM objects WHERE id = ? \
             ON CONFLICT (object_id, disk_id) DO NOTHING",
        )
        .bind(disk_id.to_string())
        .bind(object_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    async fn remove_replica(&self, object_id: Uuid, disk_id: DiskId) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        sqlx::query("DELETE FROM replicas WHERE object_id = ? AND disk_id = ?")
            .bind(object_id.to_string())
            .bind(disk_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_replicas(
        &self,
        after: Option<(Uuid, DiskId)>,
        limit: u32,
    ) -> Result<Vec<ReplicaEntry>> {
        let (after_obj, after_disk) = after
            .map(|(o, d)| (o.to_string(), d.to_string()))
            .unwrap_or_default();
        let rows = sqlx::query(
            "SELECT r.object_id, r.disk_id, o.blake3 FROM replicas r \
             JOIN objects o ON o.id = r.object_id \
             WHERE (r.object_id, r.disk_id) > (?, ?) \
             ORDER BY r.object_id, r.disk_id LIMIT ?",
        )
        .bind(after_obj)
        .bind(after_disk)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(ReplicaEntry {
                    object_id: parse_uuid(r.try_get("object_id")?)?,
                    disk_id: parse_uuid(r.try_get("disk_id")?)?,
                    blake3: r.try_get("blake3")?,
                })
            })
            .collect()
    }

    async fn list_misreplicated(
        &self,
        after: Option<Uuid>,
        limit: u32,
        alive_since: i64,
    ) -> Result<Vec<Uuid>> {
        let rows = sqlx::query(
            "SELECT o.id FROM objects o WHERE o.id > ? AND o.desired_replicas <> ( \
               SELECT COUNT(*) FROM replicas r JOIN disks d ON d.id = r.disk_id \
               WHERE r.object_id = o.id AND d.online = TRUE AND d.heartbeat_at >= ?) \
             ORDER BY o.id LIMIT ?",
        )
        .bind(after.map(|u| u.to_string()).unwrap_or_default())
        .bind(alive_since)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| parse_uuid(r.try_get("id")?)).collect()
    }
}
