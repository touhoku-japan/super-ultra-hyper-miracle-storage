use std::{str::FromStr, time::Duration};

use async_trait::async_trait;
use sqlx::{
    Row, SqliteConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteRow},
};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
    AccessKeyRecord, BucketRecord, DeleteBucketOutcome, MetadataStore, ObjectRecord, PartRecord,
    ReplicaEntry, StoredObject, TenantRecord, UploadRecord, decode_disks, decode_user_metadata,
    encode_disks, encode_user_metadata,
};
use crate::{
    disk::DiskId,
    error::{Result, StoreError},
};

/// Applied in order; each runs once, inside a transaction. Never edit a released migration.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("migrations/sqlite/0001_init.sql")),
    (2, include_str!("migrations/sqlite/0002_multipart.sql")),
];

macro_rules! object_select {
    ($where:literal) => {
        concat!(
            "SELECT id, bucket, object_key, size, etag, blake3, content_type, storage_class, ",
            "user_metadata, desired_replicas, replicas_explicit, created_at FROM objects WHERE ",
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
        let mut tx = pool.begin().await?;
        sqlx::raw_sql(*sql).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO schema_migrations (version) VALUES (?)")
            .bind(version)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::info!(version, "applied metadata migration");
    }
    Ok(())
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
         storage_class, user_metadata, desired_replicas, replicas_explicit, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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

fn tenant_from_row(r: &SqliteRow) -> Result<TenantRecord> {
    Ok(TenantRecord {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        created_at: r.try_get("created_at")?,
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
    let row = sqlx::query(object_select!("bucket = ? AND object_key = ?"))
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
        sqlx::query("SELECT id, name, created_at FROM tenants WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(tenant_from_row)
            .transpose()
    }

    async fn get_tenant_by_name(&self, name: &str) -> Result<Option<TenantRecord>> {
        sqlx::query("SELECT id, name, created_at FROM tenants WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(tenant_from_row)
            .transpose()
    }

    async fn list_tenants(&self) -> Result<Vec<TenantRecord>> {
        sqlx::query("SELECT id, name, created_at FROM tenants ORDER BY name")
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
        sqlx::query("UPDATE disks SET online = 0 WHERE node_id = ?")
            .bind(node_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn register_disk(&self, disk_id: DiskId, node_id: &str, path: &str) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        sqlx::query(
            "INSERT INTO disks (id, node_id, path, online) VALUES (?, ?, ?, 1) \
             ON CONFLICT (id) DO UPDATE SET node_id = excluded.node_id, \
             path = excluded.path, online = 1",
        )
        .bind(disk_id.to_string())
        .bind(node_id)
        .bind(path)
        .execute(&self.pool)
        .await?;
        Ok(())
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
        sqlx::query("SELECT name, owner, replicas, created_at FROM buckets WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(bucket_from_row)
            .transpose()
    }

    async fn list_buckets(&self, owner: &str) -> Result<Vec<BucketRecord>> {
        sqlx::query(
            "SELECT name, owner, replicas, created_at FROM buckets WHERE owner = ? ORDER BY name",
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
            "UPDATE objects SET desired_replicas = ? WHERE bucket = ? AND replicas_explicit = 0",
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
    ) -> Result<Option<StoredObject>> {
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
        let previous = take_object(&mut tx, &object.bucket, &object.key).await?;
        if if_absent && previous.is_some() {
            return Err(StoreError::PreconditionFailed);
        }
        insert_object(&mut tx, object, replicas).await?;
        tx.commit().await?;
        Ok(previous)
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
            "bucket = ? AND object_key >= ? AND object_key > ? ORDER BY object_key LIMIT ?"
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
    ) -> Result<(Vec<PartRecord>, Option<StoredObject>)> {
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
        let previous = take_object(&mut tx, &object.bucket, &object.key).await?;
        insert_object(&mut tx, object, replicas).await?;
        tx.commit().await?;
        Ok((parts, previous))
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

    async fn list_misreplicated(&self, after: Option<Uuid>, limit: u32) -> Result<Vec<Uuid>> {
        let rows = sqlx::query(
            "SELECT o.id FROM objects o WHERE o.id > ? AND o.desired_replicas <> ( \
               SELECT COUNT(*) FROM replicas r JOIN disks d ON d.id = r.disk_id \
               WHERE r.object_id = o.id AND d.online = 1) \
             ORDER BY o.id LIMIT ?",
        )
        .bind(after.map(|u| u.to_string()).unwrap_or_default())
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| parse_uuid(r.try_get("id")?)).collect()
    }
}
