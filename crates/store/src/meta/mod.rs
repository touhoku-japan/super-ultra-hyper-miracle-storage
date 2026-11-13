//! Metadata: buckets, objects, and which disks hold each object's copies.
//!
//! Backends implement [`MetadataStore`]. SQLite is for single-node deployments and development;
//! multi-node clusters need a shared, highly available PostgreSQL or MariaDB.

mod sqlite;

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

pub use self::sqlite::SqliteMetadata;
use crate::{
    disk::DiskId,
    error::{Result, StoreError},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketRecord {
    pub name: String,
    /// Replica count for objects without an explicit storage class; `None` = cluster default.
    pub replicas: Option<u8>,
    /// Unix time in milliseconds.
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRecord {
    /// Blob id; also names the object's files on disk. A new id is used for every write.
    pub id: Uuid,
    pub bucket: String,
    pub key: String,
    pub size: u64,
    /// Hex MD5, without the surrounding quotes S3 puts on the wire.
    pub etag: String,
    pub blake3: String,
    pub content_type: Option<String>,
    /// Storage class the client asked for, if any.
    pub storage_class: Option<String>,
    pub desired_replicas: u8,
    /// True when `desired_replicas` came from the object's storage class, so later bucket
    /// changes must not override it.
    pub replicas_explicit: bool,
    /// Unix time in milliseconds.
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredObject {
    pub object: ObjectRecord,
    /// Disks holding a copy (including disks that are currently offline).
    pub replicas: Vec<DiskId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaEntry {
    pub object_id: Uuid,
    pub disk_id: DiskId,
    pub blake3: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteBucketOutcome {
    Deleted,
    NotFound,
    NotEmpty,
}

#[async_trait]
pub trait MetadataStore: Send + Sync {
    /// Marks every disk of a node offline; called at startup before registering the disks that
    /// actually opened.
    async fn set_node_disks_offline(&self, node_id: &str) -> Result<()>;
    /// Records a disk and marks it online.
    async fn register_disk(&self, disk_id: DiskId, node_id: &str, path: &str) -> Result<()>;

    /// Returns false when the bucket already exists.
    async fn create_bucket(&self, bucket: &BucketRecord) -> Result<bool>;
    async fn get_bucket(&self, name: &str) -> Result<Option<BucketRecord>>;
    /// Sets the bucket's replica setting and updates `desired_replicas` of its objects that have
    /// no explicit storage class to `effective`. Returns false when the bucket does not exist.
    async fn set_bucket_replicas(
        &self,
        name: &str,
        replicas: Option<u8>,
        effective: u8,
    ) -> Result<bool>;
    async fn delete_bucket(&self, name: &str) -> Result<DeleteBucketOutcome>;

    /// Inserts or replaces the object at (bucket, key) and returns the object it replaced.
    /// Fails with [`StoreError::NoSuchBucket`] when the bucket does not exist.
    async fn put_object(
        &self,
        object: &ObjectRecord,
        replicas: &[DiskId],
    ) -> Result<Option<StoredObject>>;
    async fn get_object(&self, bucket: &str, key: &str) -> Result<Option<StoredObject>>;
    async fn get_object_by_id(&self, id: Uuid) -> Result<Option<StoredObject>>;
    /// Removes the object and returns what was removed.
    async fn delete_object(&self, bucket: &str, key: &str) -> Result<Option<StoredObject>>;

    /// Returns false when the object no longer exists.
    async fn add_replica(&self, object_id: Uuid, disk_id: DiskId) -> Result<bool>;
    async fn remove_replica(&self, object_id: Uuid, disk_id: DiskId) -> Result<()>;

    /// All replicas in (object_id, disk_id) order, starting after `after`.
    async fn list_replicas(
        &self,
        after: Option<(Uuid, DiskId)>,
        limit: u32,
    ) -> Result<Vec<ReplicaEntry>>;
    /// Ids of objects whose number of copies on online disks differs from `desired_replicas`,
    /// in id order, starting after `after`.
    async fn list_misreplicated(&self, after: Option<Uuid>, limit: u32) -> Result<Vec<Uuid>>;
}

/// Opens the backend named by the URL scheme.
pub async fn connect(url: &str) -> Result<Arc<dyn MetadataStore>> {
    let scheme = url.split(':').next().unwrap_or_default();
    match scheme {
        "sqlite" => Ok(Arc::new(SqliteMetadata::connect(url).await?)),
        "postgres" | "postgresql" | "mysql" | "mariadb" => Err(StoreError::Config(format!(
            "metadata backend {scheme} is not implemented yet"
        ))),
        _ => Err(StoreError::Config(format!(
            "unsupported metadata URL scheme {scheme:?} (expected sqlite, postgres or mysql)"
        ))),
    }
}
