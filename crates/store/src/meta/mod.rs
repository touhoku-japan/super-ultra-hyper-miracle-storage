//! Metadata: tenants, access keys, buckets, objects, and which disks hold each object's copies.
//!
//! Backends implement [`MetadataStore`]. SQLite is for single-node deployments and development;
//! multi-node clusters need a shared, highly available PostgreSQL or MariaDB.

mod mysql;
mod postgres;
mod sqlite;

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use uuid::Uuid;

pub use self::{mysql::MysqlMetadata, postgres::PostgresMetadata, sqlite::SqliteMetadata};
use crate::{
    disk::DiskId,
    error::{Result, StoreError},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantRecord {
    pub id: String,
    /// Unique human-readable name (e.g. the SoraHost customer id).
    pub name: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessKeyRecord {
    /// The S3 access key id.
    pub id: String,
    pub tenant_id: String,
    /// The secret key, encrypted by the server (SigV4 needs the raw secret, so it cannot be
    /// hashed). Opaque to the storage engine.
    pub secret_enc: String,
    pub active: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketRecord {
    pub name: String,
    /// Tenant id of the owner.
    pub owner: String,
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
    /// `x-amz-meta-*` headers, keys without the prefix.
    pub user_metadata: BTreeMap<String, String>,
    pub desired_replicas: u8,
    /// True when `desired_replicas` came from the object's storage class, so later bucket
    /// changes must not override it.
    pub replicas_explicit: bool,
    /// Unix time in milliseconds.
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadRecord {
    pub id: Uuid,
    pub bucket: String,
    pub key: String,
    pub content_type: Option<String>,
    pub storage_class: Option<String>,
    pub user_metadata: BTreeMap<String, String>,
    pub desired_replicas: u8,
    pub replicas_explicit: bool,
    /// Disks chosen for the parts; completing concatenates the parts on each of them locally.
    pub disks: Vec<DiskId>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartRecord {
    pub upload_id: Uuid,
    pub part_number: u32,
    pub blob_id: Uuid,
    pub size: u64,
    /// Hex MD5 of the part.
    pub etag: String,
    pub blake3: String,
    /// Disks holding a copy of this part.
    pub disks: Vec<DiskId>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskRecord {
    pub id: DiskId,
    pub node_id: String,
    pub path: String,
    pub online: bool,
    pub free_bytes: u64,
    /// Unix time in milliseconds of the owning node's last heartbeat for this disk.
    pub heartbeat_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteBucketOutcome {
    Deleted,
    NotFound,
    NotEmpty,
}

#[async_trait]
pub trait MetadataStore: Send + Sync {
    // ---- tenants and keys ----

    /// Returns false when a tenant with the same name exists.
    async fn create_tenant(&self, tenant: &TenantRecord) -> Result<bool>;
    async fn get_tenant(&self, id: &str) -> Result<Option<TenantRecord>>;
    async fn get_tenant_by_name(&self, name: &str) -> Result<Option<TenantRecord>>;
    async fn list_tenants(&self) -> Result<Vec<TenantRecord>>;
    async fn create_access_key(&self, key: &AccessKeyRecord) -> Result<()>;
    async fn get_access_key(&self, id: &str) -> Result<Option<AccessKeyRecord>>;
    async fn list_access_keys(&self, tenant_id: &str) -> Result<Vec<AccessKeyRecord>>;
    /// Returns false when the key does not exist.
    async fn set_access_key_active(&self, id: &str, active: bool) -> Result<bool>;

    // ---- disks ----

    /// Marks every disk of a node offline; called at startup before registering the disks that
    /// actually opened.
    async fn set_node_disks_offline(&self, node_id: &str) -> Result<()>;
    /// Records a disk and marks it online.
    async fn register_disk(
        &self,
        disk_id: DiskId,
        node_id: &str,
        path: &str,
        free_bytes: u64,
        now: i64,
    ) -> Result<()>;
    /// Refreshes a disk's free space and liveness.
    async fn heartbeat_disk(&self, disk_id: DiskId, free_bytes: u64, now: i64) -> Result<()>;
    /// Every disk of every node.
    async fn list_disks(&self) -> Result<Vec<DiskRecord>>;

    // ---- buckets ----

    /// Returns false when the bucket already exists.
    async fn create_bucket(&self, bucket: &BucketRecord) -> Result<bool>;
    async fn get_bucket(&self, name: &str) -> Result<Option<BucketRecord>>;
    /// Buckets owned by a tenant, by name.
    async fn list_buckets(&self, owner: &str) -> Result<Vec<BucketRecord>>;
    /// Sets the bucket's replica setting and updates `desired_replicas` of its objects that have
    /// no explicit storage class to `effective`. Returns false when the bucket does not exist.
    async fn set_bucket_replicas(
        &self,
        name: &str,
        replicas: Option<u8>,
        effective: u8,
    ) -> Result<bool>;
    async fn delete_bucket(&self, name: &str) -> Result<DeleteBucketOutcome>;

    // ---- objects ----

    /// Inserts or replaces the object at (bucket, key) and returns the object it replaced.
    /// Fails with [`StoreError::NoSuchBucket`] when the bucket does not exist, and with
    /// [`StoreError::PreconditionFailed`] when `if_absent` is set and the key exists.
    async fn put_object(
        &self,
        object: &ObjectRecord,
        replicas: &[DiskId],
        if_absent: bool,
    ) -> Result<Option<StoredObject>>;
    async fn get_object(&self, bucket: &str, key: &str) -> Result<Option<StoredObject>>;
    async fn get_object_by_id(&self, id: Uuid) -> Result<Option<StoredObject>>;
    /// Removes the object and returns what was removed.
    async fn delete_object(&self, bucket: &str, key: &str) -> Result<Option<StoredObject>>;
    /// Objects of a bucket with `key >= from` and `key > after`, in binary (UTF-8 byte) key
    /// order.
    async fn list_objects(
        &self,
        bucket: &str,
        from: &str,
        after: &str,
        limit: u32,
    ) -> Result<Vec<ObjectRecord>>;

    // ---- multipart uploads ----

    /// Fails with [`StoreError::NoSuchBucket`] when the bucket does not exist.
    async fn create_upload(&self, upload: &UploadRecord) -> Result<()>;
    async fn get_upload(&self, id: Uuid) -> Result<Option<UploadRecord>>;
    /// Uploads of a bucket under `prefix`, ordered by (key, id), after (`after_key`, `after_id`).
    async fn list_uploads(
        &self,
        bucket: &str,
        prefix: &str,
        after_key: &str,
        after_id: &str,
        limit: u32,
    ) -> Result<Vec<UploadRecord>>;
    /// Records a part, replacing any earlier part with the same number (which is returned).
    /// Fails with [`StoreError::NoSuchUpload`] when the upload no longer exists.
    async fn put_part(&self, part: &PartRecord) -> Result<Option<PartRecord>>;
    async fn list_parts(
        &self,
        upload_id: Uuid,
        after_part: u32,
        limit: u32,
    ) -> Result<Vec<PartRecord>>;
    /// Removes an upload and its parts, returning what was removed.
    async fn delete_upload(&self, id: Uuid) -> Result<Option<(UploadRecord, Vec<PartRecord>)>>;
    /// Atomically removes the upload and stores the completed object in its place. Returns the
    /// upload's parts and the object that was replaced. Fails with
    /// [`StoreError::NoSuchUpload`] when the upload was completed or aborted meanwhile.
    async fn complete_upload(
        &self,
        upload_id: Uuid,
        object: &ObjectRecord,
        replicas: &[DiskId],
    ) -> Result<(Vec<PartRecord>, Option<StoredObject>)>;

    // ---- replicas ----

    /// Returns false when the object no longer exists.
    async fn add_replica(&self, object_id: Uuid, disk_id: DiskId) -> Result<bool>;
    async fn remove_replica(&self, object_id: Uuid, disk_id: DiskId) -> Result<()>;
    /// All replicas in (object_id, disk_id) order, starting after `after`.
    async fn list_replicas(
        &self,
        after: Option<(Uuid, DiskId)>,
        limit: u32,
    ) -> Result<Vec<ReplicaEntry>>;
    /// Ids of objects whose number of copies on live disks (online, with a heartbeat newer than
    /// `alive_since`) differs from `desired_replicas`, in id order, starting after `after`.
    async fn list_misreplicated(
        &self,
        after: Option<Uuid>,
        limit: u32,
        alive_since: i64,
    ) -> Result<Vec<Uuid>>;
}

/// Opens the backend named by the URL scheme.
pub async fn connect(url: &str) -> Result<Arc<dyn MetadataStore>> {
    let scheme = url.split(':').next().unwrap_or_default();
    match scheme {
        "sqlite" => Ok(Arc::new(SqliteMetadata::connect(url).await?)),
        "postgres" | "postgresql" => Ok(Arc::new(PostgresMetadata::connect(url).await?)),
        "mysql" | "mariadb" => Ok(Arc::new(
            MysqlMetadata::connect(&url.replacen("mariadb:", "mysql:", 1)).await?,
        )),
        _ => Err(StoreError::Config(format!(
            "unsupported metadata URL scheme {scheme:?} (expected sqlite, postgres or mysql)"
        ))),
    }
}

pub(crate) fn encode_user_metadata(m: &BTreeMap<String, String>) -> String {
    serde_json::to_string(m).expect("string map serializes")
}

/// Whether a failed transaction is worth retrying: another writer won a race (unique key),
/// or the database aborted it to resolve a conflict (serialization failure, deadlock).
pub(crate) fn is_retryable(e: &StoreError) -> bool {
    let StoreError::Metadata(inner) = e else {
        return false;
    };
    let Some(sqlx::Error::Database(db)) = inner.downcast_ref::<sqlx::Error>() else {
        return false;
    };
    db.is_unique_violation() || matches!(db.code().as_deref(), Some("40001" | "40P01"))
}

pub(crate) fn encode_disks(d: &[DiskId]) -> String {
    serde_json::to_string(d).expect("uuid list serializes")
}

pub(crate) fn decode_disks(s: &str) -> Result<Vec<DiskId>> {
    serde_json::from_str(s).map_err(|e| StoreError::Metadata(Box::new(e)))
}

pub(crate) fn decode_user_metadata(s: &str) -> Result<BTreeMap<String, String>> {
    serde_json::from_str(s).map_err(|e| StoreError::Metadata(Box::new(e)))
}
