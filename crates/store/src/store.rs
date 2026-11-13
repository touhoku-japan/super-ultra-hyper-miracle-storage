use std::{
    collections::HashSet,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use tokio::{fs::File, io::AsyncRead};
use uuid::Uuid;

use crate::{
    blob::{self, CopyOutcome},
    config::StoreConfig,
    disk::{Disk, DiskId},
    error::{Result, StoreError},
    meta::{self, BucketRecord, DeleteBucketOutcome, MetadataStore, ObjectRecord, StoredObject},
    placement::{self, Candidate},
};

const MAX_KEY_LEN: usize = 1024;
const PAGE: u32 = 500;

#[derive(Debug, Default)]
pub struct PutObjectRequest {
    pub bucket: String,
    pub key: String,
    pub content_type: Option<String>,
    /// S3 storage class; overrides the bucket's replica count.
    pub storage_class: Option<String>,
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

/// The storage engine of one node: its disks plus the shared metadata store.
pub struct Store {
    config: StoreConfig,
    disks: Vec<Arc<Disk>>,
    meta: Arc<dyn MetadataStore>,
}

fn now_millis() -> i64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970");
    i64::try_from(d.as_millis()).expect("timestamp fits in i64")
}

fn validate_bucket_name(name: &str) -> Result<()> {
    let ok = (3..=63).contains(&name.len())
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
    pub async fn open(config: StoreConfig) -> Result<Self> {
        config.validate()?;
        let meta = meta::connect(&config.metadata_url).await?;
        Self::with_metadata(config, meta).await
    }

    /// Opens the configured disks. Disks that cannot be opened are logged and skipped so the
    /// node keeps serving from the rest; their copies count as missing until they return.
    pub async fn with_metadata(config: StoreConfig, meta: Arc<dyn MetadataStore>) -> Result<Self> {
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
                    meta.register_disk(disk.id, &disk.node_id, &path.to_string_lossy())
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
        })
    }

    fn disk(&self, id: DiskId) -> Option<&Arc<Disk>> {
        self.disks.iter().find(|d| d.id == id)
    }

    /// Most copies an object can have: one per node, or one per disk on a single node.
    pub fn max_replicas(&self) -> u8 {
        let nodes: HashSet<&str> = self.disks.iter().map(|d| d.node_id.as_str()).collect();
        let n = if nodes.len() > 1 {
            nodes.len()
        } else {
            self.disks.len()
        };
        u8::try_from(n).unwrap_or(u8::MAX)
    }

    fn check_replicas(&self, requested: u8) -> Result<u8> {
        let max = self.max_replicas();
        if requested == 0 || requested > max {
            return Err(StoreError::InvalidReplicaCount { requested, max });
        }
        Ok(requested)
    }

    /// Replica count for a new object: storage class > bucket setting > cluster default.
    /// Returns the count and whether it came from the storage class.
    fn resolve_replicas(
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
        Ok((self.check_replicas(n)?, explicit))
    }

    fn place(&self, count: usize, existing: &[DiskId]) -> Vec<Arc<Disk>> {
        let existing: Vec<(DiskId, &str)> = existing
            .iter()
            .filter_map(|id| self.disk(*id).map(|d| (d.id, d.node_id.as_str())))
            .collect();
        let candidates = self
            .disks
            .iter()
            .map(|d| Candidate {
                disk_id: d.id,
                node_id: &d.node_id,
                free: d.available_space(),
            })
            .collect();
        placement::choose(candidates, &existing, count)
            .into_iter()
            .filter_map(|id| self.disk(id).cloned())
            .collect()
    }

    async fn require_bucket(&self, name: &str) -> Result<BucketRecord> {
        self.meta
            .get_bucket(name)
            .await?
            .ok_or_else(|| StoreError::NoSuchBucket(name.to_owned()))
    }

    async fn remove_files(&self, stored: &StoredObject) {
        for id in &stored.replicas {
            if let Some(disk) = self.disk(*id) {
                blob::remove(disk, stored.object.id).await;
            }
        }
    }

    // ---- buckets ----

    pub async fn create_bucket(&self, name: &str, replicas: Option<u8>) -> Result<BucketRecord> {
        validate_bucket_name(name)?;
        if let Some(n) = replicas {
            self.check_replicas(n)?;
        }
        let record = BucketRecord {
            name: name.to_owned(),
            replicas,
            created_at: now_millis(),
        };
        if !self.meta.create_bucket(&record).await? {
            return Err(StoreError::BucketAlreadyExists(name.to_owned()));
        }
        Ok(record)
    }

    pub async fn get_bucket(&self, name: &str) -> Result<BucketRecord> {
        self.require_bucket(name).await
    }

    /// Changes the bucket's replica count (`None` = cluster default). Existing objects without
    /// an explicit storage class follow; [`Store::repair`] adds or removes their copies.
    pub async fn set_bucket_replicas(&self, name: &str, replicas: Option<u8>) -> Result<()> {
        let effective = self.check_replicas(replicas.unwrap_or(self.config.default_replicas))?;
        if !self
            .meta
            .set_bucket_replicas(name, replicas, effective)
            .await?
        {
            return Err(StoreError::NoSuchBucket(name.to_owned()));
        }
        Ok(())
    }

    pub async fn delete_bucket(&self, name: &str) -> Result<()> {
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
        req: PutObjectRequest,
        body: R,
    ) -> Result<ObjectRecord> {
        if req.key.is_empty() || req.key.len() > MAX_KEY_LEN {
            return Err(StoreError::InvalidObjectKey);
        }
        let bucket = self.require_bucket(&req.bucket).await?;
        let (replicas, explicit) = self.resolve_replicas(&bucket, req.storage_class.as_deref())?;
        let required = placement::ack_threshold(replicas);
        let targets = self.place(usize::from(replicas), &[]);
        if targets.len() < required {
            return Err(StoreError::WriteQuorum {
                required,
                written: targets.len(),
            });
        }

        let id = Uuid::new_v4();
        let outcome = blob::write_replicas(&targets, id, required, body).await?;
        if outcome.written.len() < usize::from(replicas) {
            tracing::warn!(
                bucket = %req.bucket, key = %req.key,
                written = outcome.written.len(), replicas,
                "object stored with fewer copies than requested; repair will add the rest"
            );
        }

        let record = ObjectRecord {
            id,
            bucket: req.bucket,
            key: req.key,
            size: outcome.size,
            etag: outcome.md5,
            blake3: outcome.blake3,
            content_type: req.content_type,
            storage_class: req.storage_class,
            desired_replicas: replicas,
            replicas_explicit: explicit,
            created_at: now_millis(),
        };
        match self.meta.put_object(&record, &outcome.written).await {
            Ok(previous) => {
                if let Some(prev) = previous {
                    self.remove_files(&prev).await;
                }
                Ok(record)
            }
            Err(e) => {
                let written = StoredObject {
                    object: record,
                    replicas: outcome.written,
                };
                self.remove_files(&written).await;
                Err(e)
            }
        }
    }

    async fn require_object(&self, bucket: &str, key: &str) -> Result<StoredObject> {
        if let Some(stored) = self.meta.get_object(bucket, key).await? {
            return Ok(stored);
        }
        self.require_bucket(bucket).await?;
        Err(StoreError::NoSuchKey {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
        })
    }

    pub async fn head_object(&self, bucket: &str, key: &str) -> Result<ObjectRecord> {
        Ok(self.require_object(bucket, key).await?.object)
    }

    /// Opens a readable copy of the object, trying each copy in turn.
    pub async fn get_object(&self, bucket: &str, key: &str) -> Result<(ObjectRecord, File)> {
        let stored = self.require_object(bucket, key).await?;
        for id in &stored.replicas {
            let Some(disk) = self.disk(*id) else { continue };
            let path = disk.object_path(stored.object.id);
            let opened = async {
                let file = File::open(&path).await?;
                let len = file.metadata().await?.len();
                Ok::<_, std::io::Error>((file, len))
            }
            .await;
            match opened {
                Ok((file, len)) if len == stored.object.size => return Ok((stored.object, file)),
                Ok((_, len)) => {
                    tracing::warn!(path = %path.display(), len, expected = stored.object.size, "replica has wrong size");
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "replica unreadable");
                }
            }
        }
        Err(StoreError::NoHealthyReplica(stored.object.id))
    }

    /// Deletes the object. Returns false when it did not exist.
    pub async fn delete_object(&self, bucket: &str, key: &str) -> Result<bool> {
        self.require_bucket(bucket).await?;
        match self.meta.delete_object(bucket, key).await? {
            Some(prev) => {
                self.remove_files(&prev).await;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Paths of the object's copies on this node's online disks.
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
            let ids = self.meta.list_misreplicated(after, PAGE).await?;
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
        let mut online: Vec<Arc<Disk>> = stored
            .replicas
            .iter()
            .filter_map(|d| self.disk(*d).cloned())
            .collect();

        if online.len() > desired {
            // Remove surplus copies from the fullest disks first.
            online.sort_by_key(|d| d.available_space());
            for disk in &online[..online.len() - desired] {
                self.meta.remove_replica(id, disk.id).await?;
                blob::remove(disk, id).await;
                report.trimmed += 1;
            }
            return Ok(());
        }

        // A copy that turns out corrupt mid-repair leaves the object short; the next pass
        // (list_misreplicated) picks it up again.
        for target in self.place(desired - online.len(), &stored.replicas) {
            loop {
                let Some(source) = online.first().cloned() else {
                    tracing::error!(object = %id, bucket = %stored.object.bucket, key = %stored.object.key,
                        "no healthy copy on an online disk; cannot repair");
                    report.unrecoverable += 1;
                    return Ok(());
                };
                match blob::copy_replica(&source, &target, id, &stored.object.blake3).await {
                    Ok(CopyOutcome::Copied) => {
                        if self.meta.add_replica(id, target.id).await? {
                            report.copied += 1;
                        } else {
                            // Object was deleted meanwhile.
                            blob::remove(&target, id).await;
                            return Ok(());
                        }
                        break;
                    }
                    Ok(CopyOutcome::SourceCorrupt) => {
                        tracing::warn!(object = %id, disk = %source.root.display(), "source copy is corrupt; dropping it");
                        self.meta.remove_replica(id, source.id).await?;
                        blob::remove(&source, id).await;
                        online.remove(0);
                    }
                    Ok(CopyOutcome::SourceUnreadable) => {
                        // Possibly transient (e.g. a failing disk); try another copy but keep
                        // the record. Scrub decides whether the copy is really gone.
                        online.remove(0);
                    }
                    Err(error) => {
                        tracing::warn!(object = %id, target = %target.root.display(), %error, "copy failed");
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}
