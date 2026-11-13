use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ops::Range,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use md5::{Digest, Md5};
use tokio::{io::AsyncRead, sync::RwLock};
use uuid::Uuid;

use crate::{
    blob::{self, CopyOutcome, Dest},
    cluster::{BoxReader, DiskRef, NoPeers, Peers},
    config::StoreConfig,
    disk::{Disk, DiskId},
    error::{Result, StoreError},
    lifecycle::{self, LifecycleRule, Transition},
    list::{self, ListPage, ListParams, PREFIX_SKIP},
    lock::{self, BucketLock},
    meta::{
        self, BucketRecord, BucketSetting, DeleteBucketOutcome, LifecycleScan, MetadataStore,
        NULL_VERSION, ObjectRecord, PartRecord, StorageUsage, StoredObject, TenantRecord,
        UploadRecord, VersioningMode, WriteCondition,
    },
    placement::{self, Candidate},
};

const MAX_KEY_LEN: usize = 1024;
/// S3's limit on the total size of user-defined metadata.
const MAX_USER_METADATA: usize = 2048;
/// S3's minimum size for every part except the last.
pub const MIN_PART_SIZE: u64 = 5 * 1024 * 1024;
pub const MAX_PART_NUMBER: u32 = 10_000;
const PAGE: u32 = 500;
/// How often nodes refresh their disks' liveness and free space in the metadata store.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// A disk whose last heartbeat is older than this is treated as offline.
const ALIVE_MS: i64 = 30_000;
/// How long a node reuses its snapshot of the cluster's disks.
const VIEW_TTL: Duration = Duration::from_secs(2);
/// Quota checks use a tenant's stored bytes as of at most this long ago, so a tenant can
/// overshoot its quota by what it writes within this window.
const USAGE_TTL: Duration = Duration::from_secs(30);

/// One disk as seen by this node.
#[derive(Debug, Clone)]
pub struct DiskInfo {
    pub id: DiskId,
    pub node_id: String,
    pub free: u64,
    /// Online and heartbeating (always true for this node's own opened disks).
    pub alive: bool,
}

/// Where the server publishes a body's checksum once the body has been read completely; the
/// store records it with the object or part. The value is opaque to the store.
pub type ChecksumSlot = Arc<std::sync::Mutex<Option<String>>>;

fn take_slot(slot: Option<&ChecksumSlot>) -> Option<String> {
    slot.and_then(|s| s.lock().ok().and_then(|mut v| v.take()))
}

/// The object a copy reads from.
#[derive(Debug, Clone, Copy)]
pub struct CopySource<'a> {
    pub bucket: &'a str,
    pub key: &'a str,
    /// A specific version; the latest when `None`.
    pub version_id: Option<&'a str>,
}

/// Lets the caller wrap a copy's source stream (e.g. to compute a checksum); gets the
/// source object.
pub type CopyWrap = Box<dyn FnOnce(&ObjectRecord, BoxReader) -> BoxReader + Send>;

/// Computes the completed object's checksum from the upload and the chosen parts (in order),
/// or rejects the completion.
pub type CompleteChecksum =
    Box<dyn FnOnce(&UploadRecord, &[&PartRecord]) -> Result<Option<String>> + Send>;

#[derive(Debug, Default)]
pub struct PutObjectRequest {
    pub bucket: String,
    pub key: String,
    pub content_type: Option<String>,
    /// S3 storage class; overrides the bucket's replica count.
    pub storage_class: Option<String>,
    /// `x-amz-meta-*` headers, keys without the prefix.
    pub user_metadata: BTreeMap<String, String>,
    /// Expected MD5 (`Content-MD5`); the write fails with [`StoreError::BadDigest`] on mismatch.
    pub content_md5: Option<[u8; 16]>,
    /// Precondition on the key's current version (`If-None-Match: *`, `If-Match`).
    pub condition: WriteCondition,
    /// The body's checksum, filled in by the server while the body streams.
    pub checksum: Option<ChecksumSlot>,
    /// For multipart uploads: checksum algorithm and type requested at creation.
    pub multipart_checksum: Option<String>,
    /// Object Lock retention for the new object (needs a lock-enabled bucket); without it the
    /// bucket's default retention applies.
    pub lock_mode: Option<String>,
    pub lock_until: Option<i64>,
    pub legal_hold: bool,
}

/// Outcome of a DeleteObject.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeleteResult {
    /// The version that was deleted, or the delete marker that was created.
    pub version_id: Option<String>,
    /// Whether a delete marker was created (or deleted).
    pub delete_marker: bool,
}

#[derive(Debug, Clone, Default)]
pub struct VersionsPage {
    pub versions: Vec<ObjectRecord>,
    /// Keys rolled up by the delimiter.
    pub common_prefixes: Vec<String>,
    pub truncated: bool,
    /// Where the next page starts (set when truncated).
    pub next_key_marker: Option<String>,
    pub next_version_id_marker: Option<String>,
}

/// One entry of a CompleteMultipartUpload request.
#[derive(Debug, Clone)]
pub struct CompletedPart {
    pub part_number: u32,
    /// ETag as sent by the client (quotes are ignored).
    pub etag: String,
}

#[derive(Debug, Clone, Default)]
pub struct PartsPage {
    pub parts: Vec<PartRecord>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct UploadsPage {
    pub uploads: Vec<UploadRecord>,
    /// Keys rolled up by the delimiter.
    pub common_prefixes: Vec<String>,
    pub truncated: bool,
    /// Where the next page starts (set when truncated).
    pub next_key_marker: Option<String>,
    pub next_upload_id_marker: Option<String>,
}

/// What one lifecycle pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleReport {
    /// Current versions expired (deleted, or hidden by a delete marker).
    pub expired: u64,
    /// Noncurrent versions permanently deleted.
    pub purged: u64,
    /// Expired object delete markers removed.
    pub markers: u64,
    /// Versions moved to another storage class.
    pub transitioned: u64,
    pub aborted: u64,
}

/// Entries fetched per metadata query while listing with a delimiter.
const LIST_BATCH: u32 = 1000;

/// The common prefix `key` rolls up into under `prefix` and `delimiter`, if any.
fn rolled_up<'a>(key: &'a str, prefix: &str, delimiter: Option<&str>) -> Option<&'a str> {
    let d = delimiter.filter(|d| !d.is_empty())?;
    let rest = key.strip_prefix(prefix)?;
    let pos = rest.find(d)?;
    Some(&key[..prefix.len() + pos + d.len()])
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScrubReport {
    pub checked: u64,
    /// Copies that were missing or did not match their hash and were dropped.
    pub removed: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RepairReport {
    pub copied: u64,
    pub trimmed: u64,
    /// Objects that are short of copies and have no healthy copy on an online disk.
    pub unrecoverable: u64,
}

/// The storage engine of one node: its own disks, the other nodes (via [`Peers`]) and the
/// shared metadata store.
pub struct Store {
    config: StoreConfig,
    disks: Vec<Arc<Disk>>,
    meta: Arc<dyn MetadataStore>,
    peers: Arc<dyn Peers>,
    view: RwLock<Option<(Instant, Arc<Vec<DiskInfo>>)>>,
    /// Logical bytes stored per tenant, for quota checks (refreshed every [`USAGE_TTL`]).
    usage: RwLock<HashMap<String, (Instant, u64)>>,
}

fn now_millis() -> i64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970");
    i64::try_from(d.as_millis()).expect("timestamp fits in i64")
}

/// Start (unix ms) of the hour containing `millis`.
pub fn hour_of(millis: i64) -> i64 {
    millis - millis.rem_euclid(3_600_000)
}

/// Version id for a new object or delete marker: time-ordered when versioning is enabled,
/// otherwise the `null` version.
fn new_version_id(mode: VersioningMode) -> String {
    match mode {
        VersioningMode::Enabled => Uuid::now_v7().simple().to_string(),
        _ => NULL_VERSION.to_owned(),
    }
}

/// Fails if Object Lock protects the version from deletion.
fn check_unlocked(o: &ObjectRecord, bypass_governance: bool) -> Result<()> {
    if o.legal_hold {
        return Err(StoreError::ObjectLocked);
    }
    if let (Some(mode), Some(until)) = (&o.lock_mode, o.lock_until)
        && until > now_millis()
        && (mode == lock::COMPLIANCE || !bypass_governance)
    {
        return Err(StoreError::ObjectLocked);
    }
    Ok(())
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return Err(StoreError::InvalidObjectKey);
    }
    Ok(())
}

/// Bucket names the server keeps for itself (e.g. the health check at `/suhms/health`).
pub const RESERVED_BUCKETS: &[&str] = &["suhms"];

fn validate_bucket_name(name: &str) -> Result<()> {
    let ok = !RESERVED_BUCKETS.contains(&name)
        && (3..=63).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name.ends_with(|c: char| c.is_ascii_alphanumeric())
        && !name.contains("..");
    if ok {
        Ok(())
    } else {
        Err(StoreError::InvalidBucketName(name.to_owned()))
    }
}

impl Store {
    /// Opens a single-node store.
    pub async fn open(config: StoreConfig) -> Result<Self> {
        Self::open_with_peers(config, Arc::new(NoPeers)).await
    }

    /// Opens a store that is part of a cluster; `peers` reaches the other nodes.
    pub async fn open_with_peers(config: StoreConfig, peers: Arc<dyn Peers>) -> Result<Self> {
        config.validate()?;
        let meta = meta::connect(&config.metadata_url).await?;
        Self::with_metadata(config, meta, peers).await
    }

    /// Opens the configured disks. Disks that cannot be opened are logged and skipped so the
    /// node keeps serving from the rest; their copies count as missing until they return.
    pub async fn with_metadata(
        config: StoreConfig,
        meta: Arc<dyn MetadataStore>,
        peers: Arc<dyn Peers>,
    ) -> Result<Self> {
        config.validate()?;
        meta.set_node_disks_offline(&config.node_id).await?;
        let mut disks = Vec::new();
        let mut seen = HashSet::new();
        for path in &config.disks {
            match Disk::open(path, &config.node_id).await {
                Ok(disk) => {
                    if !seen.insert(disk.id) {
                        return Err(StoreError::Config(format!(
                            "disk {} has the same identity as another configured disk",
                            path.display()
                        )));
                    }
                    meta.register_disk(
                        disk.id,
                        &disk.node_id,
                        &path.to_string_lossy(),
                        disk.available_space(),
                        now_millis(),
                    )
                    .await?;
                    disks.push(Arc::new(disk));
                }
                Err(error) => {
                    tracing::error!(path = %path.display(), %error, "disk unavailable; continuing without it");
                }
            }
        }
        if disks.is_empty() {
            return Err(StoreError::NoDisks);
        }
        Ok(Self {
            config,
            disks,
            meta,
            peers,
            view: RwLock::new(None),
            usage: RwLock::new(HashMap::new()),
        })
    }

    pub fn node_id(&self) -> &str {
        &self.config.node_id
    }

    /// Reports this node's disks as alive, with their current free space. Call every
    /// [`HEARTBEAT_INTERVAL`]; other nodes treat disks without a recent heartbeat as offline.
    pub async fn heartbeat(&self) -> Result<()> {
        let now = now_millis();
        for d in &self.disks {
            self.meta
                .heartbeat_disk(d.id, d.available_space(), now)
                .await?;
        }
        Ok(())
    }

    /// Every registered disk of the cluster, with liveness. Cached briefly.
    pub async fn cluster_disks(&self) -> Result<Arc<Vec<DiskInfo>>> {
        if let Some((at, view)) = &*self.view.read().await
            && at.elapsed() < VIEW_TTL
        {
            return Ok(Arc::clone(view));
        }
        let alive_since = now_millis() - ALIVE_MS;
        let infos: Vec<DiskInfo> = self
            .meta
            .list_disks()
            .await?
            .into_iter()
            .map(|r| match self.local_disk(r.id) {
                Some(d) => DiskInfo {
                    id: r.id,
                    node_id: r.node_id,
                    free: d.available_space(),
                    alive: true,
                },
                None => DiskInfo {
                    id: r.id,
                    alive: r.online
                        && r.heartbeat_at >= alive_since
                        && r.node_id != self.config.node_id,
                    node_id: r.node_id,
                    free: r.free_bytes,
                },
            })
            .collect();
        let infos = Arc::new(infos);
        *self.view.write().await = Some((Instant::now(), Arc::clone(&infos)));
        Ok(infos)
    }

    /// Drops the cached view of the cluster so the next operation re-reads it.
    pub async fn refresh_cluster(&self) {
        *self.view.write().await = None;
    }

    /// How a disk can be reached, if it is alive.
    fn reach(&self, info: &DiskInfo) -> Option<DiskRef> {
        if let Some(d) = self.local_disk(info.id) {
            return Some(DiskRef::Local(Arc::clone(d)));
        }
        if !info.alive {
            return None;
        }
        self.peers.get(&info.node_id).map(|peer| DiskRef::Remote {
            id: info.id,
            node_id: info.node_id.clone(),
            peer,
        })
    }

    /// Reachable disks among `ids`, this node's own disks first.
    async fn reachable(&self, ids: &[DiskId]) -> Result<Vec<DiskRef>> {
        let view = self.cluster_disks().await?;
        let mut refs: Vec<DiskRef> = ids
            .iter()
            .filter_map(|id| view.iter().find(|i| i.id == *id))
            .filter_map(|info| self.reach(info))
            .collect();
        refs.sort_by_key(|d| !d.is_local());
        Ok(refs)
    }

    /// Removes temp files left by interrupted writes. Call once at server startup, before
    /// accepting requests; never while another process may be writing to the same disks.
    pub async fn clear_tmp(&self) {
        for disk in &self.disks {
            if let Err(error) = disk.clear_tmp().await {
                tracing::warn!(disk = %disk.root.display(), %error, "failed to clear temp files");
            }
        }
    }

    /// One of this node's own (opened) disks.
    pub fn local_disk(&self, id: DiskId) -> Option<&Arc<Disk>> {
        self.disks.iter().find(|d| d.id == id)
    }

    fn disk(&self, id: DiskId) -> Option<&Arc<Disk>> {
        self.local_disk(id)
    }

    /// Most copies an object can have: one per node, or one per disk in a single-node
    /// deployment. Counts registered disks whether or not they are currently alive, so that a
    /// node outage degrades writes (fewer copies, repaired later) instead of rejecting them.
    pub async fn max_replicas(&self) -> Result<u8> {
        let view = self.cluster_disks().await?;
        let nodes: HashSet<&str> = view.iter().map(|d| d.node_id.as_str()).collect();
        let n = if nodes.len() > 1 {
            nodes.len()
        } else {
            view.len()
        };
        Ok(u8::try_from(n).unwrap_or(u8::MAX))
    }

    async fn check_replicas(&self, requested: u8) -> Result<u8> {
        let max = self.max_replicas().await?;
        if requested == 0 || requested > max {
            return Err(StoreError::InvalidReplicaCount { requested, max });
        }
        Ok(requested)
    }

    /// Replica count for a new object: storage class > bucket setting > cluster default.
    /// Returns the count and whether it came from the storage class.
    async fn resolve_replicas(
        &self,
        bucket: &BucketRecord,
        storage_class: Option<&str>,
    ) -> Result<(u8, bool)> {
        let (n, explicit) = match storage_class {
            Some(class) => {
                let n = *self
                    .config
                    .storage_classes
                    .get(class)
                    .ok_or_else(|| StoreError::UnknownStorageClass(class.to_owned()))?;
                (n, true)
            }
            None => (
                bucket.replicas.unwrap_or(self.config.default_replicas),
                false,
            ),
        };
        Ok((self.check_replicas(n).await?, explicit))
    }

    /// Picks reachable disks for `count` new copies, avoiding disks (and, when possible, nodes)
    /// in `existing`.
    async fn place(&self, count: usize, existing: &[DiskId]) -> Result<Vec<DiskRef>> {
        let view = self.cluster_disks().await?;
        let existing: Vec<(DiskId, &str)> = existing
            .iter()
            .filter_map(|id| view.iter().find(|i| i.id == *id))
            .map(|i| (i.id, i.node_id.as_str()))
            .collect();
        let reachable: Vec<(&DiskInfo, DiskRef)> = view
            .iter()
            .filter_map(|i| self.reach(i).map(|r| (i, r)))
            .collect();
        let candidates = reachable
            .iter()
            .map(|(i, _)| Candidate {
                disk_id: i.id,
                node_id: &i.node_id,
                free: i.free,
            })
            .collect();
        let single_node = view
            .iter()
            .map(|i| i.node_id.as_str())
            .collect::<HashSet<_>>()
            .len()
            <= 1;
        Ok(placement::choose(candidates, &existing, count, single_node)
            .into_iter()
            .filter_map(|id| {
                reachable
                    .iter()
                    .find(|(i, _)| i.id == id)
                    .map(|(_, r)| r.clone())
            })
            .collect())
    }

    async fn require_bucket(&self, name: &str) -> Result<BucketRecord> {
        self.meta
            .get_bucket(name)
            .await?
            .ok_or_else(|| StoreError::NoSuchBucket(name.to_owned()))
    }

    /// Deletes an object's files from every reachable disk. Copies on unreachable disks become
    /// orphans (they only waste space).
    async fn remove_files(&self, stored: &StoredObject) {
        match self.reachable(&stored.replicas).await {
            Ok(disks) => {
                for d in disks {
                    d.remove(Dest::Object(stored.object.id)).await;
                }
            }
            Err(error) => tracing::warn!(%error, "cannot resolve disks to remove files"),
        }
    }

    // ---- quotas ----

    /// Logical bytes the tenant stores (cached for [`USAGE_TTL`]).
    async fn stored_bytes(&self, tenant: &str) -> Result<u64> {
        if let Some((at, bytes)) = self.usage.read().await.get(tenant)
            && at.elapsed() < USAGE_TTL
        {
            return Ok(*bytes);
        }
        let bytes = self
            .meta
            .storage_usage(Some(tenant))
            .await?
            .first()
            .map_or(0, |u| u.logical_bytes);
        self.usage
            .write()
            .await
            .insert(tenant.to_owned(), (Instant::now(), bytes));
        Ok(bytes)
    }

    /// Fails with [`StoreError::QuotaExceeded`] if adding `adding` bytes would exceed the
    /// tenant's storage quota.
    async fn check_quota(&self, tenant: &str, adding: u64) -> Result<()> {
        let Some(t) = self.meta.get_tenant(tenant).await? else {
            return Ok(());
        };
        let Some(quota) = t.quota_bytes else {
            return Ok(());
        };
        let used = self.stored_bytes(tenant).await?;
        if used.saturating_add(adding) > quota {
            return Err(StoreError::QuotaExceeded);
        }
        let mut cache = self.usage.write().await;
        if let Some((_, bytes)) = cache.get_mut(tenant) {
            *bytes = bytes.saturating_add(adding);
        }
        Ok(())
    }

    // ---- tenants ----

    /// Creates a tenant (a customer account that owns buckets and access keys).
    pub async fn create_tenant(&self, name: &str) -> Result<TenantRecord> {
        if name.is_empty() || name.len() > 128 {
            return Err(StoreError::InvalidArgument(
                "tenant name must be 1-128 bytes".into(),
            ));
        }
        let record = TenantRecord {
            id: Uuid::new_v4().to_string(),
            name: name.to_owned(),
            created_at: now_millis(),
            ..TenantRecord::default()
        };
        if !self.meta.create_tenant(&record).await? {
            return Err(StoreError::TenantAlreadyExists(name.to_owned()));
        }
        Ok(record)
    }

    /// Direct access to the metadata store, for tenant/key administration.
    pub fn metadata(&self) -> &Arc<dyn MetadataStore> {
        &self.meta
    }

    // ---- buckets ----

    /// Looks up a bucket on behalf of `owner`. Another tenant's bucket is reported as
    /// [`StoreError::AccessDenied`], like S3 does.
    async fn owned_bucket(&self, owner: &str, name: &str) -> Result<BucketRecord> {
        let bucket = self.require_bucket(name).await?;
        if bucket.owner != owner {
            return Err(StoreError::AccessDenied);
        }
        Ok(bucket)
    }

    pub async fn create_bucket(
        &self,
        owner: &str,
        name: &str,
        replicas: Option<u8>,
    ) -> Result<BucketRecord> {
        validate_bucket_name(name)?;
        if let Some(n) = replicas {
            self.check_replicas(n).await?;
        }
        if let Some(t) = self.meta.get_tenant(owner).await?
            && let Some(max) = t.max_buckets
            && self.meta.list_buckets(owner).await?.len() as u64 >= max
        {
            return Err(StoreError::TooManyBuckets);
        }
        let record = BucketRecord {
            name: name.to_owned(),
            owner: owner.to_owned(),
            replicas,
            created_at: now_millis(),
            ..BucketRecord::default()
        };
        if !self.meta.create_bucket(&record).await? {
            let existing = self.meta.get_bucket(name).await?;
            return Err(match existing {
                Some(b) if b.owner == owner => StoreError::BucketAlreadyOwnedByYou(name.to_owned()),
                _ => StoreError::BucketAlreadyExists(name.to_owned()),
            });
        }
        Ok(record)
    }

    pub async fn get_bucket(&self, owner: &str, name: &str) -> Result<BucketRecord> {
        self.owned_bucket(owner, name).await
    }

    pub async fn list_buckets(&self, owner: &str) -> Result<Vec<BucketRecord>> {
        self.meta.list_buckets(owner).await
    }

    /// Changes the bucket's replica count (`None` = cluster default). Existing objects without
    /// an explicit storage class follow; [`Store::repair`] adds or removes their copies.
    /// Administrative: not scoped to a tenant.
    pub async fn set_bucket_replicas(&self, name: &str, replicas: Option<u8>) -> Result<()> {
        let effective = self
            .check_replicas(replicas.unwrap_or(self.config.default_replicas))
            .await?;
        if !self
            .meta
            .set_bucket_replicas(name, replicas, effective)
            .await?
        {
            return Err(StoreError::NoSuchBucket(name.to_owned()));
        }
        Ok(())
    }

    pub async fn delete_bucket(&self, owner: &str, name: &str) -> Result<()> {
        self.owned_bucket(owner, name).await?;
        if self.meta.list_objects(name, "", "", 1).await?.is_empty() {
            // Unfinished multipart uploads would otherwise keep the bucket alive invisibly.
            loop {
                let uploads = self.meta.list_uploads(name, "", "", "", 100).await?;
                if uploads.is_empty() {
                    break;
                }
                for u in uploads {
                    if let Some((upload, parts)) = self.meta.delete_upload(u.id).await? {
                        self.remove_upload_files(&upload, &parts).await;
                    }
                }
            }
        }
        match self.meta.delete_bucket(name).await? {
            DeleteBucketOutcome::Deleted => Ok(()),
            DeleteBucketOutcome::NotFound => Err(StoreError::NoSuchBucket(name.to_owned())),
            DeleteBucketOutcome::NotEmpty => Err(StoreError::BucketNotEmpty(name.to_owned())),
        }
    }

    // ---- objects ----

    /// Stores `body` as a new version of the object. Succeeds once a majority of the copies are
    /// durable; missing copies are filled in by [`Store::repair`].
    pub async fn put_object<R: AsyncRead + Unpin>(
        &self,
        owner: &str,
        req: PutObjectRequest,
        body: R,
    ) -> Result<ObjectRecord> {
        let bucket = self.owned_bucket(owner, &req.bucket).await?;
        self.write_object(&bucket, req, body).await
    }

    fn check_user_metadata(m: &BTreeMap<String, String>) -> Result<()> {
        let size: usize = m.iter().map(|(k, v)| k.len() + v.len()).sum();
        if size > MAX_USER_METADATA {
            return Err(StoreError::InvalidArgument(format!(
                "user metadata exceeds {MAX_USER_METADATA} bytes"
            )));
        }
        Ok(())
    }

    async fn write_object<R: AsyncRead + Unpin>(
        &self,
        bucket: &BucketRecord,
        req: PutObjectRequest,
        body: R,
    ) -> Result<ObjectRecord> {
        validate_key(&req.key)?;
        Self::check_user_metadata(&req.user_metadata)?;
        if req.condition != WriteCondition::None {
            // Cheap early exit; the metadata store re-checks atomically at commit.
            let latest = self.meta.get_object(&req.bucket, &req.key).await?;
            req.condition
                .check(latest.as_ref().map(|l| &l.object), &req.bucket, &req.key)?;
        }
        self.check_quota(&bucket.owner, 0).await?;
        let (replicas, explicit) = self
            .resolve_replicas(bucket, req.storage_class.as_deref())
            .await?;
        let required = placement::ack_threshold(replicas);
        let targets = self.place(usize::from(replicas), &[]).await?;
        if targets.len() < required {
            return Err(StoreError::WriteQuorum {
                required,
                written: targets.len(),
            });
        }

        let mode = VersioningMode::of(bucket);
        let (lock_mode, lock_until) =
            Self::retention_for(bucket, req.lock_mode.as_deref(), req.lock_until)?;
        if req.legal_hold && !lock::decode(bucket.object_lock.as_deref()).enabled {
            return Err(StoreError::InvalidRequest(
                "the bucket does not have Object Lock enabled".into(),
            ));
        }
        let id = Uuid::new_v4();
        let outcome = blob::write_replicas(&targets, Dest::Object(id), required, body).await?;
        let written = StoredObject {
            object: ObjectRecord {
                id,
                bucket: req.bucket,
                key: req.key,
                size: outcome.size,
                etag: outcome.md5,
                blake3: outcome.blake3,
                content_type: req.content_type,
                storage_class: req.storage_class,
                user_metadata: req.user_metadata,
                desired_replicas: replicas,
                replicas_explicit: explicit,
                created_at: now_millis(),
                version_id: new_version_id(mode),
                is_latest: true,
                delete_marker: false,
                noncurrent_at: None,
                lock_mode,
                lock_until,
                legal_hold: req.legal_hold,
                checksum: take_slot(req.checksum.as_ref()),
            },
            replicas: outcome.written,
        };
        if let Some(expected) = req.content_md5
            && hex::encode(expected) != written.object.etag
        {
            self.remove_files(&written).await;
            return Err(StoreError::BadDigest);
        }
        if let Err(e) = self.check_quota(&bucket.owner, written.object.size).await {
            self.remove_files(&written).await;
            return Err(e);
        }
        if written.replicas.len() < usize::from(replicas) {
            tracing::warn!(
                bucket = %written.object.bucket, key = %written.object.key,
                written = written.replicas.len(), replicas,
                "object stored with fewer copies than requested; repair will add the rest"
            );
        }
        self.commit_object(written, &req.condition, mode).await
    }

    async fn commit_object(
        &self,
        written: StoredObject,
        cond: &WriteCondition,
        mode: VersioningMode,
    ) -> Result<ObjectRecord> {
        match self
            .meta
            .put_object(&written.object, &written.replicas, cond, mode)
            .await
        {
            Ok(removed) => {
                for prev in removed {
                    self.remove_files(&prev).await;
                }
                Ok(written.object)
            }
            Err(e) => {
                self.remove_files(&written).await;
                Err(e)
            }
        }
    }

    /// Copies an object (server side). With `replace_metadata` false, the content type and
    /// user metadata come from the source; otherwise from `dest`.
    pub async fn copy_object(
        &self,
        owner: &str,
        src_bucket: &str,
        src_key: &str,
        dest: PutObjectRequest,
        replace_metadata: bool,
    ) -> Result<ObjectRecord> {
        let src = CopySource {
            bucket: src_bucket,
            key: src_key,
            version_id: None,
        };
        Ok(self
            .copy_object_with(owner, src, dest, replace_metadata, None)
            .await?
            .0)
    }

    /// Like [`Store::copy_object`], from a specific version if given, with the source stream
    /// optionally wrapped by `wrap`. Returns the new object and the source version.
    pub async fn copy_object_with(
        &self,
        owner: &str,
        src: CopySource<'_>,
        mut dest: PutObjectRequest,
        replace_metadata: bool,
        wrap: Option<CopyWrap>,
    ) -> Result<(ObjectRecord, ObjectRecord)> {
        let source = self
            .owned_version(owner, src.bucket, src.key, src.version_id)
            .await?;
        let bucket = self.owned_bucket(owner, &dest.bucket).await?;
        if src.bucket == dest.bucket
            && src.key == dest.key
            && src.version_id.is_none()
            && !replace_metadata
            && dest.storage_class.is_none()
        {
            return Err(StoreError::InvalidRequest(
                "copying an object onto itself requires changing its metadata or storage class"
                    .into(),
            ));
        }
        if !replace_metadata {
            dest.content_type.clone_from(&source.object.content_type);
            dest.user_metadata.clone_from(&source.object.user_metadata);
        }
        dest.content_md5 = None;
        let mut reader = self.read_object(&source, 0..source.object.size).await?;
        if let Some(wrap) = wrap {
            reader = wrap(&source.object, reader);
        }
        let written = self.write_object(&bucket, dest, reader).await?;
        Ok((written, source.object))
    }

    // ---- multipart uploads ----

    /// Starts a multipart upload. The disks for the parts are chosen now, so that completing
    /// the upload can concatenate the parts locally on each disk.
    pub async fn create_multipart_upload(
        &self,
        owner: &str,
        req: PutObjectRequest,
    ) -> Result<UploadRecord> {
        validate_key(&req.key)?;
        Self::check_user_metadata(&req.user_metadata)?;
        let bucket = self.owned_bucket(owner, &req.bucket).await?;
        let (replicas, explicit) = self
            .resolve_replicas(&bucket, req.storage_class.as_deref())
            .await?;
        let targets = self.place(usize::from(replicas), &[]).await?;
        let required = placement::ack_threshold(replicas);
        if targets.len() < required {
            return Err(StoreError::WriteQuorum {
                required,
                written: targets.len(),
            });
        }
        let upload = UploadRecord {
            id: Uuid::new_v4(),
            bucket: req.bucket,
            key: req.key,
            content_type: req.content_type,
            storage_class: req.storage_class,
            user_metadata: req.user_metadata,
            desired_replicas: replicas,
            replicas_explicit: explicit,
            disks: targets.iter().map(DiskRef::id).collect(),
            created_at: now_millis(),
            checksum: req.multipart_checksum,
        };
        self.meta.create_upload(&upload).await?;
        Ok(upload)
    }

    async fn owned_upload(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        upload_id: Uuid,
    ) -> Result<UploadRecord> {
        self.owned_bucket(owner, bucket).await?;
        self.meta
            .get_upload(upload_id)
            .await?
            .filter(|u| u.bucket == bucket && u.key == key)
            .ok_or(StoreError::NoSuchUpload)
    }

    /// An unfinished multipart upload of the tenant's object.
    pub async fn get_multipart_upload(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        upload_id: Uuid,
    ) -> Result<UploadRecord> {
        self.owned_upload(owner, bucket, key, upload_id).await
    }

    /// Stores one part. Re-uploading a part number replaces the earlier part.
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_part<R: AsyncRead + Unpin>(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        upload_id: Uuid,
        part_number: u32,
        content_md5: Option<[u8; 16]>,
        body: R,
    ) -> Result<PartRecord> {
        self.upload_part_with(
            owner,
            bucket,
            key,
            upload_id,
            part_number,
            content_md5,
            None,
            body,
        )
        .await
    }

    /// Like [`Store::upload_part`], recording the checksum published to `checksum`.
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_part_with<R: AsyncRead + Unpin>(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        upload_id: Uuid,
        part_number: u32,
        content_md5: Option<[u8; 16]>,
        checksum: Option<ChecksumSlot>,
        body: R,
    ) -> Result<PartRecord> {
        if !(1..=MAX_PART_NUMBER).contains(&part_number) {
            return Err(StoreError::InvalidArgument(format!(
                "part number must be between 1 and {MAX_PART_NUMBER}"
            )));
        }
        let upload = self.owned_upload(owner, bucket, key, upload_id).await?;
        let targets = self.reachable(&upload.disks).await?;
        let required = placement::ack_threshold(upload.desired_replicas);
        if targets.len() < required {
            return Err(StoreError::WriteQuorum {
                required,
                written: targets.len(),
            });
        }
        let blob_id = Uuid::new_v4();
        let dest = Dest::Part {
            upload: upload_id,
            blob: blob_id,
        };
        let outcome = blob::write_replicas(&targets, dest, required, body).await?;
        let remove = |disks: Vec<DiskId>, dest: Dest| async move {
            if let Ok(refs) = self.reachable(&disks).await {
                for d in refs {
                    d.remove(dest).await;
                }
            }
        };
        if let Some(expected) = content_md5
            && hex::encode(expected) != outcome.md5
        {
            remove(outcome.written, dest).await;
            return Err(StoreError::BadDigest);
        }
        let part = PartRecord {
            upload_id,
            part_number,
            blob_id,
            size: outcome.size,
            etag: outcome.md5,
            blake3: outcome.blake3,
            disks: outcome.written,
            created_at: now_millis(),
            checksum: take_slot(checksum.as_ref()),
        };
        match self.meta.put_part(&part).await {
            Ok(previous) => {
                if let Some(prev) = previous {
                    let old = Dest::Part {
                        upload: upload_id,
                        blob: prev.blob_id,
                    };
                    remove(prev.disks, old).await;
                }
                Ok(part)
            }
            Err(e) => {
                remove(part.disks, dest).await;
                Err(e)
            }
        }
    }

    /// Stores a part copied from (a byte range of) an existing object (version). `wrap` may
    /// wrap the source stream and fill `checksum`. Returns the part and the source version.
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_part_copy(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        upload_id: Uuid,
        part_number: u32,
        src: CopySource<'_>,
        range: Option<std::ops::Range<u64>>,
        wrap: Option<CopyWrap>,
        checksum: Option<ChecksumSlot>,
    ) -> Result<(PartRecord, ObjectRecord)> {
        let source = self
            .owned_version(owner, src.bucket, src.key, src.version_id)
            .await?;
        let range = range.unwrap_or(0..source.object.size);
        if range.start > range.end || range.end > source.object.size {
            return Err(StoreError::InvalidArgument(
                "copy range is out of bounds".into(),
            ));
        }
        let mut reader = self.read_object(&source, range).await?;
        if let Some(wrap) = wrap {
            reader = wrap(&source.object, reader);
        }
        let part = self
            .upload_part_with(
                owner,
                bucket,
                key,
                upload_id,
                part_number,
                None,
                checksum,
                reader,
            )
            .await?;
        Ok((part, source.object))
    }

    pub async fn list_parts(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        upload_id: Uuid,
        after_part: u32,
        max_parts: usize,
    ) -> Result<(UploadRecord, PartsPage)> {
        let upload = self.owned_upload(owner, bucket, key, upload_id).await?;
        let max = max_parts.min(1000);
        let limit = u32::try_from(max + 1).unwrap_or(u32::MAX);
        let mut parts = self.meta.list_parts(upload_id, after_part, limit).await?;
        let truncated = parts.len() > max;
        parts.truncate(max);
        Ok((upload, PartsPage { parts, truncated }))
    }

    /// Lists unfinished multipart uploads under `prefix` in (key, upload id) order, after
    /// (`key_marker`, `upload_id_marker`), with keys containing `delimiter` after the prefix
    /// rolled up into common prefixes.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_multipart_uploads(
        &self,
        owner: &str,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        key_marker: &str,
        upload_id_marker: &str,
        max_uploads: usize,
    ) -> Result<UploadsPage> {
        self.owned_bucket(owner, bucket).await?;
        let max = max_uploads.min(1000);
        let mut page = UploadsPage::default();
        if max == 0 {
            return Ok(page);
        }
        // Upload ids are lowercase UUIDs, which all sort before "~".
        let (mut key, mut id) = match rolled_up(key_marker, prefix, delimiter) {
            Some(cp) if cp.len() == key_marker.len() => {
                (format!("{key_marker}{PREFIX_SKIP}"), String::new())
            }
            _ if upload_id_marker.is_empty() && !key_marker.is_empty() => {
                (key_marker.to_owned(), "~".to_owned())
            }
            _ => (key_marker.to_owned(), upload_id_marker.to_owned()),
        };
        'fetch: loop {
            let batch = self
                .meta
                .list_uploads(bucket, prefix, &key, &id, LIST_BATCH)
                .await?;
            let exhausted = batch.len() < LIST_BATCH as usize;
            for u in batch {
                if !u.key.starts_with(prefix) {
                    break 'fetch;
                }
                if page.uploads.len() + page.common_prefixes.len() == max {
                    page.truncated = true;
                    break 'fetch;
                }
                if let Some(cp) = rolled_up(&u.key, prefix, delimiter) {
                    page.next_key_marker = Some(cp.to_owned());
                    page.next_upload_id_marker = None;
                    page.common_prefixes.push(cp.to_owned());
                    key = format!("{cp}{PREFIX_SKIP}");
                    id = String::new();
                    continue 'fetch;
                }
                key.clone_from(&u.key);
                id = u.id.to_string();
                page.next_key_marker = Some(u.key.clone());
                page.next_upload_id_marker = Some(id.clone());
                page.uploads.push(u);
            }
            if exhausted {
                break;
            }
        }
        if !page.truncated {
            page.next_key_marker = None;
            page.next_upload_id_marker = None;
        }
        Ok(page)
    }

    async fn remove_upload_files(&self, upload: &UploadRecord, parts: &[PartRecord]) {
        let mut disks: HashSet<DiskId> = upload.disks.iter().copied().collect();
        disks.extend(parts.iter().flat_map(|p| p.disks.iter().copied()));
        let disks: Vec<DiskId> = disks.into_iter().collect();
        if let Ok(refs) = self.reachable(&disks).await {
            for d in refs {
                d.remove_upload(upload.id).await;
            }
        }
    }

    pub async fn abort_multipart_upload(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        upload_id: Uuid,
    ) -> Result<()> {
        self.owned_upload(owner, bucket, key, upload_id).await?;
        let (upload, parts) = self
            .meta
            .delete_upload(upload_id)
            .await?
            .ok_or(StoreError::NoSuchUpload)?;
        self.remove_upload_files(&upload, &parts).await;
        Ok(())
    }

    /// Assembles the listed parts into the final object: each disk holding all parts
    /// concatenates them into an ordinary object file.
    pub async fn complete_multipart_upload(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        upload_id: Uuid,
        requested: &[CompletedPart],
    ) -> Result<ObjectRecord> {
        self.complete_multipart_upload_with(
            owner,
            bucket,
            key,
            upload_id,
            requested,
            WriteCondition::None,
            None,
        )
        .await
    }

    /// Like [`Store::complete_multipart_upload`], with a precondition on the key and a
    /// function computing the object's checksum from its parts.
    #[allow(clippy::too_many_arguments)]
    pub async fn complete_multipart_upload_with(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        upload_id: Uuid,
        requested: &[CompletedPart],
        cond: WriteCondition,
        checksum: Option<CompleteChecksum>,
    ) -> Result<ObjectRecord> {
        let upload = self.owned_upload(owner, bucket, key, upload_id).await?;
        if cond != WriteCondition::None {
            let latest = self.meta.get_object(bucket, key).await?;
            cond.check(latest.as_ref().map(|l| &l.object), bucket, key)?;
        }
        let bucket_rec = self.require_bucket(bucket).await?;
        let mode = VersioningMode::of(&bucket_rec);
        let (lock_mode, lock_until) = Self::retention_for(&bucket_rec, None, None)?;
        self.check_quota(owner, 0).await?;
        if requested.is_empty() {
            return Err(StoreError::InvalidRequest(
                "at least one part must be specified".into(),
            ));
        }
        if requested
            .windows(2)
            .any(|w| w[0].part_number >= w[1].part_number)
        {
            return Err(StoreError::InvalidPartOrder);
        }

        let mut stored = std::collections::HashMap::new();
        let mut after = 0;
        loop {
            let page = self.meta.list_parts(upload_id, after, 1000).await?;
            let Some(last) = page.last() else { break };
            after = last.part_number;
            for p in page {
                stored.insert(p.part_number, p);
            }
        }
        let mut chosen: Vec<&PartRecord> = Vec::with_capacity(requested.len());
        for r in requested {
            let part = stored.get(&r.part_number).ok_or_else(|| {
                StoreError::InvalidPart(format!("part {} was not uploaded", r.part_number))
            })?;
            if r.etag.trim_matches('"') != part.etag {
                return Err(StoreError::InvalidPart(format!(
                    "ETag of part {} does not match",
                    r.part_number
                )));
            }
            chosen.push(part);
        }
        if chosen[..chosen.len() - 1]
            .iter()
            .any(|p| p.size < MIN_PART_SIZE)
        {
            return Err(StoreError::EntityTooSmall);
        }

        // S3's multipart ETag: MD5 of the concatenated binary part MD5s, plus "-<count>".
        let mut md5 = Md5::new();
        for p in &chosen {
            md5.update(hex::decode(&p.etag).map_err(|e| StoreError::Metadata(Box::new(e)))?);
        }
        let etag = format!("{}-{}", hex::encode(md5.finalize()), chosen.len());
        let object_checksum = match checksum {
            Some(f) => f(&upload, &chosen)?,
            None => None,
        };
        let size: u64 = chosen.iter().map(|p| p.size).sum();
        self.check_quota(owner, size).await?;
        let blobs: Vec<(Uuid, String)> = chosen
            .iter()
            .map(|p| (p.blob_id, p.blake3.clone()))
            .collect();

        let object_id = Uuid::new_v4();
        let holders: Vec<DiskId> = upload
            .disks
            .iter()
            .filter(|d| chosen.iter().all(|p| p.disks.contains(d)))
            .copied()
            .collect();
        let candidates = self.reachable(&holders).await?;
        let results = futures::future::join_all(candidates.iter().map(|disk| {
            let blobs = blobs.clone();
            async move {
                let r = disk.concat(upload_id, blobs, object_id).await;
                (disk.clone(), r)
            }
        }))
        .await;
        let mut written: Vec<(DiskId, String)> = Vec::new();
        for (disk, r) in results {
            match r {
                Ok(hash) => written.push((disk.id(), hash)),
                Err(error) => {
                    tracing::warn!(disk = %disk.label(), %error, "cannot assemble upload on disk");
                }
            }
        }
        // Every part was verified on every disk, so all results must agree.
        let blake3 = written.first().map(|(_, h)| h.clone()).unwrap_or_default();
        let replicas: Vec<DiskId> = written
            .iter()
            .filter(|(_, h)| *h == blake3)
            .map(|(d, _)| *d)
            .collect();
        let new_object = StoredObject {
            object: ObjectRecord {
                id: object_id,
                bucket: upload.bucket.clone(),
                key: upload.key.clone(),
                size,
                etag,
                blake3,
                content_type: upload.content_type.clone(),
                storage_class: upload.storage_class.clone(),
                user_metadata: upload.user_metadata.clone(),
                desired_replicas: upload.desired_replicas,
                replicas_explicit: upload.replicas_explicit,
                created_at: now_millis(),
                version_id: new_version_id(mode),
                is_latest: true,
                delete_marker: false,
                noncurrent_at: None,
                lock_mode,
                lock_until,
                legal_hold: false,
                checksum: object_checksum,
            },
            replicas: written.iter().map(|(d, _)| *d).collect(),
        };
        let required = placement::ack_threshold(upload.desired_replicas);
        if replicas.len() < required || replicas.len() != new_object.replicas.len() {
            self.remove_files(&new_object).await;
            return Err(StoreError::WriteQuorum {
                required,
                written: replicas.len(),
            });
        }

        match self
            .meta
            .complete_upload(
                upload_id,
                &new_object.object,
                &new_object.replicas,
                &cond,
                mode,
            )
            .await
        {
            Ok((parts, removed)) => {
                for prev in removed {
                    self.remove_files(&prev).await;
                }
                self.remove_upload_files(&upload, &parts).await;
                Ok(new_object.object)
            }
            Err(e) => {
                self.remove_files(&new_object).await;
                Err(e)
            }
        }
    }

    /// The latest version of a key; a delete marker counts as no object.
    async fn require_object(&self, bucket: &str, key: &str) -> Result<StoredObject> {
        if let Some(stored) = self.meta.get_object(bucket, key).await?
            && !stored.object.delete_marker
        {
            return Ok(stored);
        }
        self.require_bucket(bucket).await?;
        Err(StoreError::NoSuchKey {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
        })
    }

    async fn owned_object(&self, owner: &str, bucket: &str, key: &str) -> Result<StoredObject> {
        self.owned_bucket(owner, bucket).await?;
        self.require_object(bucket, key).await
    }

    pub async fn head_object(&self, owner: &str, bucket: &str, key: &str) -> Result<ObjectRecord> {
        Ok(self.owned_object(owner, bucket, key).await?.object)
    }

    /// Looks up an object for reading; open its contents with [`Store::read_object`].
    pub async fn get_object(&self, owner: &str, bucket: &str, key: &str) -> Result<StoredObject> {
        self.owned_object(owner, bucket, key).await
    }

    /// Opens `range` of an object, from a copy on this node if there is one, otherwise from
    /// another node; unreadable copies are skipped.
    pub async fn read_object(&self, stored: &StoredObject, range: Range<u64>) -> Result<BoxReader> {
        if range.start > range.end || range.end > stored.object.size {
            return Err(StoreError::InvalidArgument("range is out of bounds".into()));
        }
        for disk in self.reachable(&stored.replicas).await? {
            match disk
                .read(stored.object.id, stored.object.size, range.clone())
                .await
            {
                Ok(reader) => return Ok(reader),
                Err(error) => {
                    tracing::warn!(disk = %disk.label(), object = %stored.object.id, %error, "replica unreadable");
                }
            }
        }
        Err(StoreError::NoHealthyReplica(stored.object.id))
    }

    /// Deletes the key. In a versioned bucket this adds a delete marker (the data stays as a
    /// noncurrent version); otherwise the object is removed.
    pub async fn delete_object(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
    ) -> Result<DeleteResult> {
        let b = self.owned_bucket(owner, bucket).await?;
        let mode = VersioningMode::of(&b);
        if mode == VersioningMode::Unversioned {
            if let Some(prev) = self.meta.delete_object(bucket, key).await? {
                self.remove_files(&prev).await;
            }
            return Ok(DeleteResult::default());
        }
        let marker = ObjectRecord {
            id: Uuid::new_v4(),
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            created_at: now_millis(),
            version_id: new_version_id(mode),
            is_latest: true,
            delete_marker: true,
            ..ObjectRecord::default()
        };
        for prev in self
            .meta
            .put_object(&marker, &[], &WriteCondition::None, mode)
            .await?
        {
            self.remove_files(&prev).await;
        }
        Ok(DeleteResult {
            version_id: Some(marker.version_id),
            delete_marker: true,
        })
    }

    /// Permanently deletes one version, unless Object Lock protects it. A GOVERNANCE
    /// retention can be bypassed with `bypass_governance`; COMPLIANCE and legal holds cannot.
    pub async fn delete_object_version(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        version_id: &str,
        bypass_governance: bool,
    ) -> Result<DeleteResult> {
        self.owned_bucket(owner, bucket).await?;
        let Some(v) = self
            .meta
            .get_object_version(bucket, key, version_id)
            .await?
        else {
            // Deleting a missing version succeeds, as in S3.
            return Ok(DeleteResult {
                version_id: Some(version_id.to_owned()),
                delete_marker: false,
            });
        };
        check_unlocked(&v.object, bypass_governance)?;
        if let Some(removed) = self
            .meta
            .delete_object_version(bucket, key, version_id)
            .await?
        {
            self.remove_files(&removed).await;
        }
        Ok(DeleteResult {
            version_id: Some(version_id.to_owned()),
            delete_marker: v.object.delete_marker,
        })
    }

    /// A specific version for reading. Asking for a delete marker is
    /// [`StoreError::MethodNotAllowed`], as in S3.
    pub async fn get_object_version(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<StoredObject> {
        self.owned_bucket(owner, bucket).await?;
        let v = self
            .meta
            .get_object_version(bucket, key, version_id)
            .await?
            .ok_or_else(|| StoreError::NoSuchKey {
                bucket: bucket.to_owned(),
                key: key.to_owned(),
            })?;
        if v.object.delete_marker {
            return Err(StoreError::MethodNotAllowed);
        }
        Ok(v)
    }

    /// Lists every version (and delete marker) under `prefix`, newest first per key, after
    /// (`key_marker`, `version_marker`), with keys containing `delimiter` after the prefix
    /// rolled up into common prefixes.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_object_versions(
        &self,
        owner: &str,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        key_marker: &str,
        version_marker: Option<&str>,
        max: usize,
    ) -> Result<VersionsPage> {
        self.owned_bucket(owner, bucket).await?;
        let max = max.min(1000);
        let mut page = VersionsPage::default();
        if max == 0 {
            return Ok(page);
        }
        // Resume point (key, created_at, id): versions after it in (key, newest first) order.
        let mut cursor: (String, i64, String) = match rolled_up(key_marker, prefix, delimiter) {
            Some(cp) if cp.len() == key_marker.len() => (
                format!("{key_marker}{PREFIX_SKIP}"),
                i64::MIN,
                String::new(),
            ),
            _ => {
                let resume = match version_marker {
                    Some(v) if !key_marker.is_empty() => self
                        .meta
                        .get_object_version(bucket, key_marker, v)
                        .await?
                        .map(|o| (o.object.created_at, o.object.id.to_string())),
                    _ => None,
                };
                let (created, id) = resume.unwrap_or((i64::MIN, String::new()));
                (key_marker.to_owned(), created, id)
            }
        };
        'fetch: loop {
            let batch = self
                .meta
                .list_object_versions(bucket, prefix, (&cursor.0, cursor.1, &cursor.2), LIST_BATCH)
                .await?;
            let exhausted = batch.len() < LIST_BATCH as usize;
            for o in batch {
                if !o.key.starts_with(prefix) {
                    break 'fetch;
                }
                if page.versions.len() + page.common_prefixes.len() == max {
                    page.truncated = true;
                    break 'fetch;
                }
                if let Some(cp) = rolled_up(&o.key, prefix, delimiter) {
                    page.next_key_marker = Some(cp.to_owned());
                    page.next_version_id_marker = None;
                    page.common_prefixes.push(cp.to_owned());
                    cursor = (format!("{cp}{PREFIX_SKIP}"), i64::MIN, String::new());
                    continue 'fetch;
                }
                cursor = (o.key.clone(), o.created_at, o.id.to_string());
                page.next_key_marker = Some(o.key.clone());
                page.next_version_id_marker = Some(o.version_id.clone());
                page.versions.push(o);
            }
            if exhausted {
                break;
            }
        }
        if !page.truncated {
            page.next_key_marker = None;
            page.next_version_id_marker = None;
        }
        Ok(page)
    }

    // ---- bucket configuration: versioning, lifecycle, object lock, CORS ----

    /// Turns versioning on (`Enabled`) or pauses it (`Suspended`). It cannot be turned off
    /// again, and buckets with Object Lock must stay `Enabled`.
    pub async fn set_versioning(&self, owner: &str, bucket: &str, status: &str) -> Result<()> {
        let b = self.owned_bucket(owner, bucket).await?;
        if !matches!(status, "Enabled" | "Suspended") {
            return Err(StoreError::InvalidArgument(format!(
                "invalid versioning status {status}"
            )));
        }
        if status == "Suspended" && lock::decode(b.object_lock.as_deref()).enabled {
            return Err(StoreError::InvalidRequest(
                "versioning cannot be suspended on a bucket with Object Lock".into(),
            ));
        }
        self.meta
            .set_bucket_setting(bucket, BucketSetting::Versioning, Some(status))
            .await?;
        Ok(())
    }

    /// Enables Object Lock on a bucket (which also enables versioning); used at creation.
    pub async fn enable_object_lock(&self, owner: &str, bucket: &str) -> Result<()> {
        self.owned_bucket(owner, bucket).await?;
        let cfg = BucketLock {
            enabled: true,
            ..BucketLock::default()
        };
        self.meta
            .set_bucket_setting(bucket, BucketSetting::ObjectLock, Some(&lock::encode(&cfg)))
            .await?;
        self.meta
            .set_bucket_setting(bucket, BucketSetting::Versioning, Some("Enabled"))
            .await?;
        Ok(())
    }

    /// Sets the default retention of a lock-enabled bucket (`None` removes it).
    pub async fn set_default_retention(
        &self,
        owner: &str,
        bucket: &str,
        default: Option<(String, u32)>,
    ) -> Result<()> {
        let b = self.owned_bucket(owner, bucket).await?;
        let mut cfg = lock::decode(b.object_lock.as_deref());
        if !cfg.enabled {
            return Err(StoreError::InvalidRequest(
                "the bucket does not have Object Lock enabled".into(),
            ));
        }
        if let Some((mode, _)) = &default
            && !lock::valid_mode(mode)
        {
            return Err(StoreError::InvalidArgument(format!(
                "invalid retention mode {mode}"
            )));
        }
        cfg.default_mode = default.as_ref().map(|(m, _)| m.clone());
        cfg.default_days = default.map(|(_, d)| d);
        self.meta
            .set_bucket_setting(bucket, BucketSetting::ObjectLock, Some(&lock::encode(&cfg)))
            .await?;
        Ok(())
    }

    /// The bucket's Object Lock configuration.
    pub async fn object_lock_config(&self, owner: &str, bucket: &str) -> Result<BucketLock> {
        let b = self.owned_bucket(owner, bucket).await?;
        Ok(lock::decode(b.object_lock.as_deref()))
    }

    /// Retention for a new object: explicit, else the bucket default, else none.
    fn retention_for(
        bucket: &BucketRecord,
        mode: Option<&str>,
        until: Option<i64>,
    ) -> Result<(Option<String>, Option<i64>)> {
        let cfg = lock::decode(bucket.object_lock.as_deref());
        match (mode, until) {
            (Some(m), Some(u)) => {
                if !cfg.enabled {
                    return Err(StoreError::InvalidRequest(
                        "the bucket does not have Object Lock enabled".into(),
                    ));
                }
                if !lock::valid_mode(m) {
                    return Err(StoreError::InvalidArgument(format!(
                        "invalid retention mode {m}"
                    )));
                }
                Ok((Some(m.to_owned()), Some(u)))
            }
            (None, None) => match (cfg.default_mode, cfg.default_days) {
                (Some(m), Some(days)) if cfg.enabled => {
                    Ok((Some(m), Some(now_millis() + i64::from(days) * 86_400_000)))
                }
                _ => Ok((None, None)),
            },
            _ => Err(StoreError::InvalidArgument(
                "retention needs both a mode and a retain-until date".into(),
            )),
        }
    }

    async fn owned_version(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
    ) -> Result<StoredObject> {
        match version_id {
            Some(v) => self.get_object_version(owner, bucket, key, v).await,
            None => self.owned_object(owner, bucket, key).await,
        }
    }

    /// Sets an object version's retention. COMPLIANCE retention can only be extended;
    /// GOVERNANCE can be shortened or removed only with `bypass_governance`.
    pub async fn set_object_retention(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        retention: Option<(String, i64)>,
        bypass_governance: bool,
    ) -> Result<()> {
        let b = self.owned_bucket(owner, bucket).await?;
        if !lock::decode(b.object_lock.as_deref()).enabled {
            return Err(StoreError::InvalidRequest(
                "the bucket does not have Object Lock enabled".into(),
            ));
        }
        let v = self.owned_version(owner, bucket, key, version_id).await?;
        let o = &v.object;
        let now = now_millis();
        if let (Some(mode), Some(until)) = (&o.lock_mode, o.lock_until)
            && until > now
        {
            let weakens = match &retention {
                None => true,
                Some((m, u)) => *u < until || (mode == lock::COMPLIANCE && m != lock::COMPLIANCE),
            };
            if weakens && (mode == lock::COMPLIANCE || !bypass_governance) {
                return Err(StoreError::ObjectLocked);
            }
        }
        if let Some((m, _)) = &retention
            && !lock::valid_mode(m)
        {
            return Err(StoreError::InvalidArgument(format!(
                "invalid retention mode {m}"
            )));
        }
        let (mode, until) = match retention {
            Some((m, u)) => (Some(m), Some(u)),
            None => (None, None),
        };
        self.meta
            .set_object_lock(o.id, mode.as_deref(), until)
            .await?;
        Ok(())
    }

    pub async fn set_legal_hold(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        on: bool,
    ) -> Result<()> {
        let b = self.owned_bucket(owner, bucket).await?;
        if !lock::decode(b.object_lock.as_deref()).enabled {
            return Err(StoreError::InvalidRequest(
                "the bucket does not have Object Lock enabled".into(),
            ));
        }
        let v = self.owned_version(owner, bucket, key, version_id).await?;
        self.meta.set_legal_hold(v.object.id, on).await?;
        Ok(())
    }

    /// An object version's retention and legal hold.
    pub async fn object_lock_state(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
    ) -> Result<ObjectRecord> {
        Ok(self
            .owned_version(owner, bucket, key, version_id)
            .await?
            .object)
    }

    pub async fn set_lifecycle(
        &self,
        owner: &str,
        bucket: &str,
        rules: Option<&[LifecycleRule]>,
    ) -> Result<()> {
        self.owned_bucket(owner, bucket).await?;
        for rule in rules.unwrap_or_default() {
            for t in rule.transitions.iter().chain(&rule.noncurrent_transitions) {
                let n = *self
                    .config
                    .storage_classes
                    .get(&t.storage_class)
                    .ok_or_else(|| StoreError::UnknownStorageClass(t.storage_class.clone()))?;
                self.check_replicas(n).await?;
            }
        }
        let value = rules.map(lifecycle::encode);
        self.meta
            .set_bucket_setting(bucket, BucketSetting::Lifecycle, value.as_deref())
            .await?;
        Ok(())
    }

    pub async fn lifecycle(&self, owner: &str, bucket: &str) -> Result<Option<Vec<LifecycleRule>>> {
        let b = self.owned_bucket(owner, bucket).await?;
        Ok(b.lifecycle.as_deref().map(lifecycle::decode))
    }

    /// Stores the bucket's CORS rules (opaque JSON, interpreted by the server).
    pub async fn set_cors(&self, owner: &str, bucket: &str, cors: Option<&str>) -> Result<()> {
        self.owned_bucket(owner, bucket).await?;
        self.meta
            .set_bucket_setting(bucket, BucketSetting::Cors, cors)
            .await?;
        Ok(())
    }

    /// Applies every bucket's lifecycle rules once.
    pub async fn apply_lifecycle(&self) -> Result<LifecycleReport> {
        let now = now_millis();
        let mut report = LifecycleReport::default();
        for b in self.meta.list_all_buckets().await? {
            let Some(rules) = b.lifecycle.as_deref().map(lifecycle::decode) else {
                continue;
            };
            for rule in rules.iter().filter(|r| r.enabled) {
                let days = |d: u32| now - i64::from(d) * 86_400_000;
                report.transitioned += self
                    .lifecycle_transitions(&b.name, &rule.prefix, false, &rule.transitions, now)
                    .await?;
                report.transitioned += self
                    .lifecycle_transitions(
                        &b.name,
                        &rule.prefix,
                        true,
                        &rule.noncurrent_transitions,
                        now,
                    )
                    .await?;
                if let Some(d) = rule.expiration_days {
                    for o in self
                        .lifecycle_scan(&b.name, &rule.prefix, LifecycleScan::Current, days(d))
                        .await?
                    {
                        self.delete_object(&b.owner, &b.name, &o.key).await?;
                        report.expired += 1;
                    }
                }
                if let Some(d) = rule.noncurrent_days {
                    for o in self
                        .lifecycle_scan(&b.name, &rule.prefix, LifecycleScan::Noncurrent, days(d))
                        .await?
                    {
                        match self
                            .delete_object_version(&b.owner, &b.name, &o.key, &o.version_id, false)
                            .await
                        {
                            Ok(_) => report.purged += 1,
                            Err(StoreError::ObjectLocked) => {}
                            Err(e) => return Err(e),
                        }
                    }
                }
                if rule.expired_object_delete_marker {
                    // After noncurrent versions are purged, so markers they leave alone go
                    // in the same pass.
                    for o in self
                        .lifecycle_scan(&b.name, &rule.prefix, LifecycleScan::DeleteMarkers, now)
                        .await?
                    {
                        if self.only_version(&b.name, &o).await? {
                            self.delete_object_version(
                                &b.owner,
                                &b.name,
                                &o.key,
                                &o.version_id,
                                false,
                            )
                            .await?;
                            report.markers += 1;
                        }
                    }
                }
                if let Some(d) = rule.abort_multipart_days {
                    let cutoff = days(d);
                    let uploads = self
                        .meta
                        .list_uploads(&b.name, &rule.prefix, "", "", 1000)
                        .await?;
                    for u in uploads.into_iter().filter(|u| u.created_at < cutoff) {
                        if let Some((upload, parts)) = self.meta.delete_upload(u.id).await? {
                            self.remove_upload_files(&upload, &parts).await;
                            report.aborted += 1;
                        }
                    }
                }
            }
        }
        Ok(report)
    }

    /// Whether `o` is the only version of its key.
    async fn only_version(&self, bucket: &str, o: &ObjectRecord) -> Result<bool> {
        let versions = self
            .meta
            .list_object_versions(bucket, &o.key, (&o.key, i64::MAX, ""), 2)
            .await?;
        Ok(versions
            .iter()
            .filter(|v| v.key == o.key)
            .all(|v| v.id == o.id))
    }

    /// Moves versions old enough for a transition to its storage class (the one with the
    /// most days that applies). Returns how many versions changed class.
    async fn lifecycle_transitions(
        &self,
        bucket: &str,
        prefix: &str,
        noncurrent: bool,
        transitions: &[Transition],
        now: i64,
    ) -> Result<u64> {
        let mut sorted: Vec<&Transition> = transitions.iter().collect();
        sorted.sort_by_key(|t| std::cmp::Reverse(t.days));
        let mut moved = 0;
        // Versions older than this belong to a transition with more days.
        let mut older_cutoff = i64::MIN;
        for t in sorted {
            let Some(&replicas) = self.config.storage_classes.get(&t.storage_class) else {
                tracing::warn!(bucket, class = %t.storage_class, "lifecycle transition to unknown storage class");
                continue;
            };
            let scan = if noncurrent {
                LifecycleScan::Noncurrent
            } else {
                LifecycleScan::Current
            };
            let cutoff = now - i64::from(t.days) * 86_400_000;
            for o in self.lifecycle_scan(bucket, prefix, scan, cutoff).await? {
                let age_from = if noncurrent {
                    o.noncurrent_at.unwrap_or(o.created_at)
                } else {
                    o.created_at
                };
                if age_from < older_cutoff || o.storage_class.as_deref() == Some(&t.storage_class) {
                    continue;
                }
                if self
                    .meta
                    .set_object_storage_class(o.id, &t.storage_class, replicas)
                    .await?
                {
                    moved += 1;
                }
            }
            older_cutoff = cutoff;
        }
        Ok(moved)
    }

    async fn lifecycle_scan(
        &self,
        bucket: &str,
        prefix: &str,
        scan: LifecycleScan,
        before: i64,
    ) -> Result<Vec<ObjectRecord>> {
        let mut out = Vec::new();
        let (mut after_key, mut after_id) = (String::new(), String::new());
        loop {
            let page = self
                .meta
                .lifecycle_candidates(bucket, prefix, scan, before, (&after_key, &after_id), PAGE)
                .await?;
            let Some(last) = page.last() else { break };
            after_key.clone_from(&last.key);
            after_id = last.id.to_string();
            let done = page.iter().any(|o| !o.key.starts_with(prefix));
            out.extend(page.into_iter().filter(|o| o.key.starts_with(prefix)));
            if done {
                break;
            }
        }
        Ok(out)
    }

    /// Lists a bucket the way S3's ListObjects does: keys under `prefix`, with keys that share
    /// a `delimiter`-terminated prefix rolled up into one common prefix.
    pub async fn list_objects(&self, owner: &str, params: &ListParams) -> Result<ListPage> {
        self.owned_bucket(owner, &params.bucket).await?;
        list::list(self.meta.as_ref(), params).await
    }

    /// Looks up an object for an anonymous reader: only in buckets marked public-read.
    pub async fn get_public_object(&self, bucket: &str, key: &str) -> Result<StoredObject> {
        let b = self.require_bucket(bucket).await?;
        if !b.public_read {
            return Err(StoreError::AccessDenied);
        }
        self.require_object(bucket, key).await
    }

    /// Marks a bucket public-read or private (administrative).
    pub async fn set_bucket_public(&self, name: &str, public_read: bool) -> Result<()> {
        if !self.meta.set_bucket_public(name, public_read).await? {
            return Err(StoreError::NoSuchBucket(name.to_owned()));
        }
        Ok(())
    }

    /// Aborts multipart uploads older than `max_age`, deleting their parts. Returns how many.
    pub async fn abort_stale_uploads(&self, max_age: Duration) -> Result<u64> {
        let cutoff = now_millis() - i64::try_from(max_age.as_millis()).unwrap_or(i64::MAX);
        let mut aborted = 0;
        loop {
            let stale = self.meta.list_stale_uploads(cutoff, 100).await?;
            if stale.is_empty() {
                return Ok(aborted);
            }
            for u in stale {
                if let Some((upload, parts)) = self.meta.delete_upload(u.id).await? {
                    self.remove_upload_files(&upload, &parts).await;
                    aborted += 1;
                }
            }
        }
    }

    /// Records every tenant's stored bytes in the current hour of the usage meter.
    pub async fn snapshot_storage(&self) -> Result<Vec<StorageUsage>> {
        let usage = self.meta.storage_usage(None).await?;
        self.meta
            .record_storage(hour_of(now_millis()), &usage)
            .await?;
        Ok(usage)
    }

    /// Disks recorded as holding the object (administrative, not scoped to a tenant).
    pub async fn object_replicas(&self, bucket: &str, key: &str) -> Result<Vec<DiskId>> {
        Ok(self.require_object(bucket, key).await?.replicas)
    }

    /// Paths of the object's copies on this node's online disks.
    /// Administrative: not scoped to a tenant.
    pub async fn object_locations(&self, bucket: &str, key: &str) -> Result<Vec<PathBuf>> {
        let stored = self.require_object(bucket, key).await?;
        Ok(stored
            .replicas
            .iter()
            .filter_map(|id| self.disk(*id))
            .map(|d| d.object_path(stored.object.id))
            .collect())
    }

    // ---- maintenance ----

    /// Re-hashes every copy on this node's disks and drops copies that are missing or corrupt,
    /// so that [`Store::repair`] replaces them.
    pub async fn scrub(&self) -> Result<ScrubReport> {
        let mut report = ScrubReport::default();
        let mut after = None;
        loop {
            let page = self.meta.list_replicas(after, PAGE).await?;
            let Some(last) = page.last() else { break };
            after = Some((last.object_id, last.disk_id));
            for entry in &page {
                let Some(disk) = self.disk(entry.disk_id) else {
                    continue;
                };
                report.checked += 1;
                let path = disk.object_path(entry.object_id);
                let bad = match blob::hash_file(&path).await {
                    Ok((_, hash)) => hash != entry.blake3,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                    Err(error) => {
                        // Possibly a failing disk rather than a bad file; leave it for now.
                        tracing::warn!(path = %path.display(), %error, "cannot read replica during scrub");
                        false
                    }
                };
                if bad {
                    tracing::warn!(path = %path.display(), "dropping missing or corrupt replica");
                    self.meta
                        .remove_replica(entry.object_id, entry.disk_id)
                        .await?;
                    blob::remove(disk, entry.object_id).await;
                    report.removed += 1;
                }
            }
        }
        Ok(report)
    }

    /// Brings every object's number of copies on online disks to its desired count, copying
    /// from verified healthy copies or removing surplus ones.
    pub async fn repair(&self) -> Result<RepairReport> {
        let mut report = RepairReport::default();
        let mut after = None;
        loop {
            let ids = self
                .meta
                .list_misreplicated(after, PAGE, now_millis() - ALIVE_MS)
                .await?;
            let Some(last) = ids.last() else { break };
            after = Some(*last);
            for id in ids {
                if let Some(stored) = self.meta.get_object_by_id(id).await? {
                    self.repair_object(&stored, &mut report).await?;
                }
            }
        }
        Ok(report)
    }

    async fn repair_object(&self, stored: &StoredObject, report: &mut RepairReport) -> Result<()> {
        let id = stored.object.id;
        let desired = usize::from(stored.object.desired_replicas);
        let mut online = self.reachable(&stored.replicas).await?;

        if online.len() > desired {
            // Remove surplus copies from the fullest disks first.
            let view = self.cluster_disks().await?;
            let free = |d: &DiskRef| view.iter().find(|i| i.id == d.id()).map_or(0, |i| i.free);
            online.sort_by_key(free);
            for disk in &online[..online.len() - desired] {
                self.meta.remove_replica(id, disk.id()).await?;
                disk.remove(Dest::Object(id)).await;
                report.trimmed += 1;
            }
            return Ok(());
        }

        // A copy that turns out corrupt mid-repair leaves the object short; the next pass
        // (list_misreplicated) picks it up again.
        for target in self.place(desired - online.len(), &stored.replicas).await? {
            loop {
                let Some(source) = online.first().cloned() else {
                    tracing::error!(object = %id, bucket = %stored.object.bucket, key = %stored.object.key,
                        "no healthy copy on a reachable disk; cannot repair");
                    report.unrecoverable += 1;
                    return Ok(());
                };
                match blob::copy_replica(
                    &source,
                    &target,
                    id,
                    stored.object.size,
                    &stored.object.blake3,
                )
                .await
                {
                    Ok(CopyOutcome::Copied) => {
                        if self.meta.add_replica(id, target.id()).await? {
                            report.copied += 1;
                        } else {
                            // Object was deleted meanwhile.
                            target.remove(Dest::Object(id)).await;
                            return Ok(());
                        }
                        break;
                    }
                    Ok(CopyOutcome::SourceCorrupt) => {
                        tracing::warn!(object = %id, disk = %source.label(), "source copy is corrupt; dropping it");
                        self.meta.remove_replica(id, source.id()).await?;
                        source.remove(Dest::Object(id)).await;
                        online.remove(0);
                    }
                    Ok(CopyOutcome::SourceUnreadable) => {
                        // Possibly transient (e.g. a failing disk); try another copy but keep
                        // the record. Scrub decides whether the copy is really gone.
                        online.remove(0);
                    }
                    Err(error) => {
                        tracing::warn!(object = %id, target = %target.label(), %error, "copy failed");
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}
