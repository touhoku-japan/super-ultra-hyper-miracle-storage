use std::{str::FromStr, time::Duration};

use async_trait::async_trait;
use sqlx::{
    Row, SqliteConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteRow},
};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
    BucketRecord, DeleteBucketOutcome, MetadataStore, ObjectRecord, ReplicaEntry, StoredObject,
};
use crate::{
    disk::DiskId,
    error::{Result, StoreError},
};

const SCHEMA: &str = include_str!("sqlite_schema.sql");

macro_rules! object_select {
    ($where:literal) => {
        concat!(
            "SELECT id, bucket, object_key, size, etag, blake3, content_type, storage_class, ",
            "desired_replicas, replicas_explicit, created_at FROM objects WHERE ",
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
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        Ok(Self {
            pool,
            write_lock: Mutex::new(()),
        })
    }
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
        desired_replicas: to_u8(row.try_get("desired_replicas")?)?,
        replicas_explicit: row.try_get("replicas_explicit")?,
        created_at: row.try_get("created_at")?,
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
            "INSERT INTO buckets (name, replicas, created_at) VALUES (?, ?, ?) \
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(&bucket.name)
        .bind(bucket.replicas.map(i64::from))
        .bind(bucket.created_at)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    async fn get_bucket(&self, name: &str) -> Result<Option<BucketRecord>> {
        let row = sqlx::query("SELECT name, replicas, created_at FROM buckets WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| {
            Ok(BucketRecord {
                name: r.try_get("name")?,
                replicas: r
                    .try_get::<Option<i64>, _>("replicas")?
                    .map(to_u8)
                    .transpose()?,
                created_at: r.try_get("created_at")?,
            })
        })
        .transpose()
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

        let previous = load_by_key(&mut tx, &object.bucket, &object.key).await?;
        if let Some(prev) = &previous {
            sqlx::query("DELETE FROM objects WHERE id = ?")
                .bind(prev.object.id.to_string())
                .execute(&mut *tx)
                .await?;
        }

        sqlx::query(
            "INSERT INTO objects (id, bucket, object_key, size, etag, blake3, content_type, \
             storage_class, desired_replicas, replicas_explicit, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(object.id.to_string())
        .bind(&object.bucket)
        .bind(&object.key)
        .bind(i64::try_from(object.size).map_err(|e| StoreError::Metadata(Box::new(e)))?)
        .bind(&object.etag)
        .bind(&object.blake3)
        .bind(&object.content_type)
        .bind(&object.storage_class)
        .bind(i64::from(object.desired_replicas))
        .bind(object.replicas_explicit)
        .bind(object.created_at)
        .execute(&mut *tx)
        .await?;
        for disk in replicas {
            sqlx::query("INSERT INTO replicas (object_id, disk_id) VALUES (?, ?)")
                .bind(object.id.to_string())
                .bind(disk.to_string())
                .execute(&mut *tx)
                .await?;
        }
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
        let previous = load_by_key(&mut tx, bucket, key).await?;
        if let Some(prev) = &previous {
            sqlx::query("DELETE FROM objects WHERE id = ?")
                .bind(prev.object.id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(previous)
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
