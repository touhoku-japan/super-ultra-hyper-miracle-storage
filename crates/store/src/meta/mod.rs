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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TenantRecord {
    pub id: String,
    /// Unique human-readable name (e.g. the SoraHost customer id).
    pub name: String,
    pub created_at: i64,
    /// Suspended tenants (e.g. unpaid) cannot use the S3 API.
    pub suspended: bool,
    /// Maximum total size of the tenant's objects (logical bytes, not counting replicas).
    pub quota_bytes: Option<u64>,
    pub max_buckets: Option<u64>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BucketRecord {
    pub name: String,
    /// Tenant id of the owner.
    pub owner: String,
    /// Replica count for objects without an explicit storage class; `None` = cluster default.
    pub replicas: Option<u8>,
    /// Unix time in milliseconds.
    pub created_at: i64,
    /// Anyone may GET/HEAD objects (only if the server allows public buckets).
    pub public_read: bool,
    /// `Enabled` or `Suspended`; `None` for a bucket that never had versioning.
    pub versioning: Option<String>,
    /// Lifecycle rules (JSON, see `crate::lifecycle`).
    pub lifecycle: Option<String>,
    /// CORS rules (JSON, opaque to the store).
    pub cors: Option<String>,
    /// Object Lock configuration (JSON, see `crate::lock`).
    pub object_lock: Option<String>,
}

/// The version id of objects written while versioning was off or suspended.
pub const NULL_VERSION: &str = "null";

/// How a write treats the key's existing versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersioningMode {
    /// The previous object is replaced (deleted).
    Unversioned,
    /// The previous latest version is kept as a noncurrent version.
    Enabled,
    /// Like `Enabled`, but the new version is the `null` version, replacing any older one.
    Suspended,
}

impl VersioningMode {
    pub fn of(bucket: &BucketRecord) -> Self {
        match bucket.versioning.as_deref() {
            Some("Enabled") => Self::Enabled,
            Some("Suspended") => Self::Suspended,
            _ => Self::Unversioned,
        }
    }
}

/// A precondition on the key's current (latest) version, checked atomically with a write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum WriteCondition {
    #[default]
    None,
    /// `If-None-Match: *`: the key must not exist.
    Absent,
    /// `If-Match`: the key must exist and have this ETag (hex, without quotes); `None` matches
    /// any ETag (`If-Match: *`).
    Matches(Option<String>),
}

impl WriteCondition {
    /// Checks the condition against the key's latest version (a delete marker counts as no
    /// object).
    pub fn check(&self, latest: Option<&ObjectRecord>, bucket: &str, key: &str) -> Result<()> {
        let current = latest.filter(|o| !o.delete_marker);
        match (self, current) {
            (Self::None, _) | (Self::Absent, None) => Ok(()),
            (Self::Absent, Some(_)) => Err(StoreError::PreconditionFailed),
            (Self::Matches(_), None) => Err(StoreError::NoSuchKey {
                bucket: bucket.to_owned(),
                key: key.to_owned(),
            }),
            (Self::Matches(etag), Some(o)) => match etag {
                Some(e) if *e != o.etag => Err(StoreError::PreconditionFailed),
                _ => Ok(()),
            },
        }
    }
}

/// Which versions a lifecycle scan returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleScan {
    /// Latest versions (not delete markers) created before the cutoff.
    Current,
    /// Noncurrent versions that became noncurrent before the cutoff.
    Noncurrent,
    /// Delete markers that are the latest version, created before the cutoff.
    DeleteMarkers,
}

/// Bucket settings stored as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketSetting {
    Versioning,
    Lifecycle,
    Cors,
    ObjectLock,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    /// [`NULL_VERSION`] unless written while versioning was enabled.
    pub version_id: String,
    pub is_latest: bool,
    /// A delete marker has no data (and no replicas).
    pub delete_marker: bool,
    /// When this version stopped being the latest.
    pub noncurrent_at: Option<i64>,
    /// Object Lock retention: `GOVERNANCE` or `COMPLIANCE`, until `lock_until` (unix ms).
    pub lock_mode: Option<String>,
    pub lock_until: Option<i64>,
    pub legal_hold: bool,
    /// Checksum of the content (`x-amz-checksum-*`), opaque to the store (JSON written by
    /// the server).
    pub checksum: Option<String>,
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
    /// Checksum algorithm and type requested at creation (opaque to the store).
    pub checksum: Option<String>,
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
    /// The part's checksum (opaque to the store).
    pub checksum: Option<String>,
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

/// Requests and traffic of one tenant in one hour, to be added to the meter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageDelta {
    pub tenant_id: String,
    /// Unix ms of the start of the hour.
    pub hour: i64,
    /// PUT, COPY, POST and LIST requests.
    pub class_a: u64,
    /// GET, HEAD and other requests.
    pub class_b: u64,
    pub ingress_bytes: u64,
    pub egress_bytes: u64,
}

/// What a tenant stores right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageUsage {
    pub tenant_id: String,
    pub objects: u64,
    /// Sum of object sizes.
    pub logical_bytes: u64,
    /// Sum of object sizes times their replica counts (disk space used).
    pub stored_bytes: u64,
}

/// One hour of a tenant's meter.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct UsageRow {
    pub hour: i64,
    pub class_a_requests: u64,
    pub class_b_requests: u64,
    pub ingress_bytes: u64,
    pub egress_bytes: u64,
    pub stored_objects: u64,
    pub logical_bytes: u64,
    pub stored_bytes: u64,
}

/// Changes to a tenant's settings; `None` leaves a field unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TenantUpdate {
    pub suspended: Option<bool>,
    pub quota_bytes: Option<Option<u64>>,
    pub max_buckets: Option<Option<u64>>,
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
    /// Returns false when the tenant does not exist.
    async fn update_tenant(&self, id: &str, update: &TenantUpdate) -> Result<bool>;
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
    /// Returns false when the bucket does not exist.
    async fn set_bucket_public(&self, name: &str, public_read: bool) -> Result<bool>;
    /// Returns false when the bucket does not exist.
    async fn set_bucket_setting(
        &self,
        name: &str,
        setting: BucketSetting,
        value: Option<&str>,
    ) -> Result<bool>;
    /// Every bucket of every tenant (for background jobs such as lifecycle).
    async fn list_all_buckets(&self) -> Result<Vec<BucketRecord>>;

    // ---- objects ----

    /// Stores `object` as the latest version of its key; earlier versions are kept or removed
    /// according to `mode`. Returns the versions that were removed (their files must be
    /// deleted). Fails with [`StoreError::NoSuchBucket`] when the bucket does not exist, and
    /// with [`StoreError::PreconditionFailed`] (or `NoSuchKey`) when `cond` does not hold.
    async fn put_object(
        &self,
        object: &ObjectRecord,
        replicas: &[DiskId],
        cond: &WriteCondition,
        mode: VersioningMode,
    ) -> Result<Vec<StoredObject>>;
    /// A specific version of a key.
    async fn get_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Option<StoredObject>>;
    /// Permanently removes one version; if it was the latest, the newest remaining version
    /// becomes the latest.
    async fn delete_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Option<StoredObject>>;
    /// All versions (including delete markers) under `prefix`, ordered by key, newest first,
    /// after the version (`after_key`, `after_created_at`, `after_id`).
    async fn list_object_versions(
        &self,
        bucket: &str,
        prefix: &str,
        after: (&str, i64, &str),
        limit: u32,
    ) -> Result<Vec<ObjectRecord>>;
    /// Lifecycle candidates under `prefix`, in key order after `after` (key, id), as selected
    /// by `scan` with `before` as the cutoff.
    async fn lifecycle_candidates(
        &self,
        bucket: &str,
        prefix: &str,
        scan: LifecycleScan,
        before: i64,
        after: (&str, &str),
        limit: u32,
    ) -> Result<Vec<ObjectRecord>>;
    /// Returns false when the object (version) does not exist.
    async fn set_object_lock(
        &self,
        object_id: Uuid,
        mode: Option<&str>,
        until: Option<i64>,
    ) -> Result<bool>;
    async fn set_legal_hold(&self, object_id: Uuid, on: bool) -> Result<bool>;
    /// Changes an object version's storage class and desired replica count (lifecycle
    /// transition); repair then adds or removes copies. Returns false when it no longer exists.
    async fn set_object_storage_class(
        &self,
        object_id: Uuid,
        storage_class: &str,
        replicas: u8,
    ) -> Result<bool>;
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
    /// [`StoreError::NoSuchUpload`] when the upload was completed or aborted meanwhile, and
    /// like [`MetadataStore::put_object`] when `cond` does not hold.
    async fn complete_upload(
        &self,
        upload_id: Uuid,
        object: &ObjectRecord,
        replicas: &[DiskId],
        cond: &WriteCondition,
        mode: VersioningMode,
    ) -> Result<(Vec<PartRecord>, Vec<StoredObject>)>;

    /// Uploads started before `created_before` (unix ms), oldest first.
    async fn list_stale_uploads(
        &self,
        created_before: i64,
        limit: u32,
    ) -> Result<Vec<UploadRecord>>;

    // ---- usage metering ----

    /// What each tenant (or only `tenant_id`) stores now. Tenants without objects are omitted.
    async fn storage_usage(&self, tenant_id: Option<&str>) -> Result<Vec<StorageUsage>>;
    /// Adds requests and traffic to the hourly meter.
    async fn add_usage(&self, deltas: &[UsageDelta]) -> Result<()>;
    /// Records what each tenant stores in `hour` (overwriting an earlier snapshot of that hour).
    async fn record_storage(&self, hour: i64, usage: &[StorageUsage]) -> Result<()>;
    /// A tenant's meter for hours in `[from, to)`.
    async fn usage(&self, tenant_id: &str, from: i64, to: i64) -> Result<Vec<UsageRow>>;

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
