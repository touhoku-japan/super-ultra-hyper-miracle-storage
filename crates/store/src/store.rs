use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use md5::{Digest, Md5};
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt},
};
use uuid::Uuid;

use crate::{
    blob::{self, CopyOutcome, Dest},
    config::StoreConfig,
    disk::{Disk, DiskId},
    error::{Result, StoreError},
    list::{self, ListPage, ListParams},
    meta::{
        self, BucketRecord, DeleteBucketOutcome, MetadataStore, ObjectRecord, PartRecord,
        StoredObject, TenantRecord, UploadRecord,
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
    /// `If-None-Match: *`: fail with [`StoreError::PreconditionFailed`] if the key exists.
    pub if_absent: bool,
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
    pub truncated: bool,
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

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return Err(StoreError::InvalidObjectKey);
    }
    Ok(())
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

    /// Removes temp files left by interrupted writes. Call once at server startup, before
    /// accepting requests; never while another process may be writing to the same disks.
    pub async fn clear_tmp(&self) {
        for disk in &self.disks {
            if let Err(error) = disk.clear_tmp().await {
                tracing::warn!(disk = %disk.root.display(), %error, "failed to clear temp files");
            }
        }
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
            self.check_replicas(n)?;
        }
        let record = BucketRecord {
            name: name.to_owned(),
            owner: owner.to_owned(),
            replicas,
            created_at: now_millis(),
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
        if req.if_absent && self.meta.get_object(&req.bucket, &req.key).await?.is_some() {
            // Cheap early exit; the metadata store re-checks atomically at commit.
            return Err(StoreError::PreconditionFailed);
        }
        let (replicas, explicit) = self.resolve_replicas(bucket, req.storage_class.as_deref())?;
        let required = placement::ack_threshold(replicas);
        let targets = self.place(usize::from(replicas), &[]);
        if targets.len() < required {
            return Err(StoreError::WriteQuorum {
                required,
                written: targets.len(),
            });
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
            },
            replicas: outcome.written,
        };
        if let Some(expected) = req.content_md5
            && hex::encode(expected) != written.object.etag
        {
            self.remove_files(&written).await;
            return Err(StoreError::BadDigest);
        }
        if written.replicas.len() < usize::from(replicas) {
            tracing::warn!(
                bucket = %written.object.bucket, key = %written.object.key,
                written = written.replicas.len(), replicas,
                "object stored with fewer copies than requested; repair will add the rest"
            );
        }
        self.commit_object(written, req.if_absent).await
    }

    async fn commit_object(&self, written: StoredObject, if_absent: bool) -> Result<ObjectRecord> {
        match self
            .meta
            .put_object(&written.object, &written.replicas, if_absent)
            .await
        {
            Ok(previous) => {
                if let Some(prev) = previous {
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
        mut dest: PutObjectRequest,
        replace_metadata: bool,
    ) -> Result<ObjectRecord> {
        let src = self.owned_object(owner, src_bucket, src_key).await?;
        let bucket = self.owned_bucket(owner, &dest.bucket).await?;
        if src_bucket == dest.bucket
            && src_key == dest.key
            && !replace_metadata
            && dest.storage_class.is_none()
        {
            return Err(StoreError::InvalidRequest(
                "copying an object onto itself requires changing its metadata or storage class"
                    .into(),
            ));
        }
        if !replace_metadata {
            dest.content_type.clone_from(&src.object.content_type);
            dest.user_metadata.clone_from(&src.object.user_metadata);
        }
        dest.content_md5 = None;
        let (_, file) = self.open_stored(src).await?;
        self.write_object(&bucket, dest, file).await
    }

    /// Opens a readable copy of an object restricted to `range` (whole object when `None`).
    async fn open_range(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        range: Option<std::ops::Range<u64>>,
    ) -> Result<(ObjectRecord, tokio::io::Take<File>)> {
        let stored = self.owned_object(owner, bucket, key).await?;
        let (rec, mut file) = self.open_stored(stored).await?;
        let range = range.unwrap_or(0..rec.size);
        if range.start > range.end || range.end > rec.size {
            return Err(StoreError::InvalidArgument(
                "copy range is out of bounds".into(),
            ));
        }
        file.seek(std::io::SeekFrom::Start(range.start)).await?;
        Ok((rec, file.take(range.end - range.start)))
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
        let (replicas, explicit) = self.resolve_replicas(&bucket, req.storage_class.as_deref())?;
        let targets = self.place(usize::from(replicas), &[]);
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
            disks: targets.iter().map(|d| d.id).collect(),
            created_at: now_millis(),
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
        if !(1..=MAX_PART_NUMBER).contains(&part_number) {
            return Err(StoreError::InvalidArgument(format!(
                "part number must be between 1 and {MAX_PART_NUMBER}"
            )));
        }
        let upload = self.owned_upload(owner, bucket, key, upload_id).await?;
        let targets: Vec<Arc<Disk>> = upload
            .disks
            .iter()
            .filter_map(|d| self.disk(*d).cloned())
            .collect();
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
            for d in disks {
                if let Some(disk) = self.disk(d) {
                    blob::remove_path(&dest.path(disk)).await;
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

    /// Stores a part copied from (a byte range of) an existing object.
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_part_copy(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
        upload_id: Uuid,
        part_number: u32,
        src_bucket: &str,
        src_key: &str,
        range: Option<std::ops::Range<u64>>,
    ) -> Result<PartRecord> {
        let (_, reader) = self.open_range(owner, src_bucket, src_key, range).await?;
        self.upload_part(owner, bucket, key, upload_id, part_number, None, reader)
            .await
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

    pub async fn list_multipart_uploads(
        &self,
        owner: &str,
        bucket: &str,
        prefix: &str,
        after_key: &str,
        after_id: &str,
        max_uploads: usize,
    ) -> Result<UploadsPage> {
        self.owned_bucket(owner, bucket).await?;
        let max = max_uploads.min(1000);
        let limit = u32::try_from(max + 1).unwrap_or(u32::MAX);
        let mut uploads = self
            .meta
            .list_uploads(bucket, prefix, after_key, after_id, limit)
            .await?;
        let truncated = uploads.len() > max;
        uploads.truncate(max);
        Ok(UploadsPage { uploads, truncated })
    }

    async fn remove_upload_files(&self, upload: &UploadRecord, parts: &[PartRecord]) {
        let mut disks: HashSet<DiskId> = upload.disks.iter().copied().collect();
        disks.extend(parts.iter().flat_map(|p| p.disks.iter().copied()));
        for id in disks {
            if let Some(disk) = self.disk(id) {
                blob::remove_upload_dir(disk, upload.id).await;
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
        let upload = self.owned_upload(owner, bucket, key, upload_id).await?;
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
        let size: u64 = chosen.iter().map(|p| p.size).sum();
        let blobs: Vec<(Uuid, String)> = chosen
            .iter()
            .map(|p| (p.blob_id, p.blake3.clone()))
            .collect();

        let object_id = Uuid::new_v4();
        let candidates: Vec<Arc<Disk>> = upload
            .disks
            .iter()
            .filter(|d| chosen.iter().all(|p| p.disks.contains(d)))
            .filter_map(|d| self.disk(*d).cloned())
            .collect();
        let results = futures::future::join_all(candidates.iter().map(|disk| {
            let blobs = &blobs;
            async move {
                let r = blob::concat_parts(disk, upload_id, blobs, object_id).await;
                (Arc::clone(disk), r)
            }
        }))
        .await;
        let mut written: Vec<(DiskId, String)> = Vec::new();
        for (disk, r) in results {
            match r {
                Ok(hash) => written.push((disk.id, hash)),
                Err(error) => {
                    tracing::warn!(disk = %disk.root.display(), %error, "cannot assemble upload on disk");
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
            .complete_upload(upload_id, &new_object.object, &new_object.replicas)
            .await
        {
            Ok((parts, previous)) => {
                if let Some(prev) = previous {
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

    async fn owned_object(&self, owner: &str, bucket: &str, key: &str) -> Result<StoredObject> {
        self.owned_bucket(owner, bucket).await?;
        self.require_object(bucket, key).await
    }

    pub async fn head_object(&self, owner: &str, bucket: &str, key: &str) -> Result<ObjectRecord> {
        Ok(self.owned_object(owner, bucket, key).await?.object)
    }

    /// Opens a readable copy of the object, trying each copy in turn.
    pub async fn get_object(
        &self,
        owner: &str,
        bucket: &str,
        key: &str,
    ) -> Result<(ObjectRecord, File)> {
        let stored = self.owned_object(owner, bucket, key).await?;
        self.open_stored(stored).await
    }

    pub(crate) async fn open_stored(&self, stored: StoredObject) -> Result<(ObjectRecord, File)> {
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
    pub async fn delete_object(&self, owner: &str, bucket: &str, key: &str) -> Result<bool> {
        self.owned_bucket(owner, bucket).await?;
        match self.meta.delete_object(bucket, key).await? {
            Some(prev) => {
                self.remove_files(&prev).await;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Lists a bucket the way S3's ListObjects does: keys under `prefix`, with keys that share
    /// a `delimiter`-terminated prefix rolled up into one common prefix.
    pub async fn list_objects(&self, owner: &str, params: &ListParams) -> Result<ListPage> {
        self.owned_bucket(owner, &params.bucket).await?;
        list::list(self.meta.as_ref(), params).await
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
