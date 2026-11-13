//! The S3 API, implemented on top of the storage engine via `s3s`.
//!
//! `s3s` parses requests, verifies SigV4 signatures (including aws-chunked payloads and
//! `x-amz-content-sha256`) and renders XML. Every operation here first maps the verified access
//! key to its tenant; the store then enforces that tenants only see their own buckets.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use async_trait::async_trait;
use base64::{
    Engine,
    engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL},
};
use futures::TryStreamExt;
use s3s::{
    S3, S3Error, S3ErrorCode, S3Request, S3Response, S3Result,
    auth::{S3Auth, SecretKey},
    dto::{
        AbortIncompleteMultipartUpload, AbortMultipartUploadInput, AbortMultipartUploadOutput,
        Bucket, BucketLocationConstraint, BucketVersioningStatus, CORSRule, Checksum, CommonPrefix,
        CompleteMultipartUploadInput, CompleteMultipartUploadOutput, CopyObjectInput,
        CopyObjectOutput, CopyObjectResult, CopyPartResult, CopySource, CreateBucketInput,
        CreateBucketOutput, CreateMultipartUploadInput, CreateMultipartUploadOutput,
        DefaultRetention, DeleteBucketCorsInput, DeleteBucketCorsOutput, DeleteBucketInput,
        DeleteBucketLifecycleInput, DeleteBucketLifecycleOutput, DeleteBucketOutput,
        DeleteMarkerEntry, DeleteObjectInput, DeleteObjectOutput, DeleteObjectsInput,
        DeleteObjectsOutput, DeletedObject, ETag, ETagCondition, ExpirationStatus,
        GetBucketCorsInput, GetBucketCorsOutput, GetBucketLifecycleConfigurationInput,
        GetBucketLifecycleConfigurationOutput, GetBucketLocationInput, GetBucketLocationOutput,
        GetBucketVersioningInput, GetBucketVersioningOutput, GetObjectInput,
        GetObjectLegalHoldInput, GetObjectLegalHoldOutput, GetObjectLockConfigurationInput,
        GetObjectLockConfigurationOutput, GetObjectOutput, GetObjectRetentionInput,
        GetObjectRetentionOutput, HeadBucketInput, HeadBucketOutput, HeadObjectInput,
        HeadObjectOutput, Initiator, LifecycleExpiration, LifecycleRule as S3LifecycleRule,
        ListBucketsInput, ListBucketsOutput, ListMultipartUploadsInput, ListMultipartUploadsOutput,
        ListObjectVersionsInput, ListObjectVersionsOutput, ListObjectsInput, ListObjectsOutput,
        ListObjectsV2Input, ListObjectsV2Output, ListPartsInput, ListPartsOutput, MultipartUpload,
        NoncurrentVersionExpiration, Object, ObjectLockConfiguration, ObjectLockEnabled,
        ObjectLockLegalHold, ObjectLockLegalHoldStatus, ObjectLockRetention,
        ObjectLockRetentionMode, ObjectLockRule, ObjectStorageClass, ObjectVersion,
        ObjectVersionStorageClass, Owner, Part, PutBucketCorsInput, PutBucketCorsOutput,
        PutBucketLifecycleConfigurationInput, PutBucketLifecycleConfigurationOutput,
        PutBucketVersioningInput, PutBucketVersioningOutput, PutObjectInput,
        PutObjectLegalHoldInput, PutObjectLegalHoldOutput, PutObjectLockConfigurationInput,
        PutObjectLockConfigurationOutput, PutObjectOutput, PutObjectRetentionInput,
        PutObjectRetentionOutput, StorageClass, StreamingBlob, Timestamp, UploadPartCopyInput,
        UploadPartCopyOutput, UploadPartInput, UploadPartOutput,
    },
    s3_error,
};
use std::sync::atomic::Ordering;

use suhms_store::{
    CompletedPart, ListPage, ListParams, ListStart, NULL_VERSION, ObjectRecord, PutObjectRequest,
    Store, StoreError, lifecycle::LifecycleRule, meta::StoredObject,
};
use tokio::io::AsyncRead;
use tokio_util::io::{ReaderStream, StreamReader};
use uuid::Uuid;

use crate::{
    checksum,
    crypto::SecretCipher,
    tenancy::{Class, Tenancy},
};

const READ_CHUNK: usize = 256 * 1024;

/// Resolves access keys for SigV4 verification.
pub struct KeyStore {
    tenancy: Arc<Tenancy>,
    cipher: Arc<SecretCipher>,
}

impl KeyStore {
    pub fn new(tenancy: Arc<Tenancy>, cipher: Arc<SecretCipher>) -> Self {
        Self { tenancy, cipher }
    }
}

/// Lets anonymous requests through only for object reads, and only when public buckets are
/// allowed; the handlers then check that the bucket really is public.
pub struct PublicAccess {
    pub allow_public: bool,
}

#[async_trait]
impl s3s::access::S3Access for PublicAccess {
    async fn check(&self, cx: &mut s3s::access::S3AccessContext<'_>) -> S3Result<()> {
        if cx.credentials().is_some() {
            return Ok(());
        }
        if self.allow_public && matches!(cx.s3_op().name(), "GetObject" | "HeadObject") {
            return Ok(());
        }
        Err(s3_error!(AccessDenied, "Signature is required"))
    }
}

#[async_trait]
impl S3Auth for KeyStore {
    async fn get_secret_key(&self, access_key: &str) -> S3Result<SecretKey> {
        let key = self
            .tenancy
            .directory
            .key(access_key)
            .await
            .map_err(internal)?
            .filter(|k| k.active)
            .ok_or_else(|| s3_error!(InvalidAccessKeyId))?;
        let secret = self.cipher.decrypt(&key.id, &key.secret_enc).map_err(|e| {
            tracing::error!(access_key, error = %e, "cannot decrypt access key secret");
            s3_error!(InternalError)
        })?;
        Ok(SecretKey::from(secret))
    }
}

pub struct S3Service {
    store: Arc<Store>,
    tenancy: Arc<Tenancy>,
    region: String,
}

impl S3Service {
    pub fn new(store: Arc<Store>, tenancy: Arc<Tenancy>, region: String) -> Self {
        Self {
            store,
            tenancy,
            region,
        }
    }

    /// Tenant id of the (already signature-verified) caller, after checking that the tenant is
    /// active and within its rate limit. Counts the request for billing.
    async fn tenant<T>(&self, req: &S3Request<T>, class: Class) -> S3Result<String> {
        let cred = req
            .credentials
            .as_ref()
            .ok_or_else(|| s3_error!(AccessDenied, "Signature is required"))?;
        let t = &self.tenancy;
        let key = t
            .directory
            .key(&cred.access_key)
            .await
            .map_err(internal)?
            .filter(|k| k.active)
            .ok_or_else(|| s3_error!(InvalidAccessKeyId))?;
        let tenant = t
            .directory
            .tenant(&key.tenant_id)
            .await
            .map_err(internal)?
            .ok_or_else(|| s3_error!(InvalidAccessKeyId))?;
        if tenant.suspended {
            t.meter.rejected_total.fetch_add(1, Ordering::Relaxed);
            return Err(s3_error!(AccessDenied, "This account is suspended"));
        }
        if !t.limiter.allow(&key.id) {
            t.meter.rejected_total.fetch_add(1, Ordering::Relaxed);
            return Err(s3_error!(SlowDown));
        }
        t.meter.request(&tenant.id, class);
        Ok(tenant.id)
    }

    /// For GetObject/HeadObject: the caller's tenant, or `None` for an anonymous reader (only
    /// let through by [`PublicAccess`] when public buckets are allowed).
    async fn reader<T>(&self, req: &S3Request<T>) -> S3Result<Option<String>> {
        if req.credentials.is_none() && self.tenancy.allow_public {
            return Ok(None);
        }
        self.tenant(req, Class::B).await.map(Some)
    }

    /// Looks up an object for reading; returns it with the tenant its traffic is billed to.
    async fn lookup_for_read(
        &self,
        reader: Option<&str>,
        bucket: &str,
        key: &str,
    ) -> S3Result<(StoredObject, String)> {
        if let Some(tenant) = reader {
            let stored = self
                .store
                .get_object(tenant, bucket, key)
                .await
                .map_err(s3_err)?;
            return Ok((stored, tenant.to_owned()));
        }
        let stored = self
            .store
            .get_public_object(bucket, key)
            .await
            .map_err(s3_err)?;
        // Anonymous traffic is billed to the bucket's owner.
        let owner = self
            .store
            .metadata()
            .get_bucket(bucket)
            .await
            .map_err(internal)?
            .map(|b| b.owner)
            .unwrap_or_default();
        self.tenancy.meter.request(&owner, Class::B);
        Ok((stored, owner))
    }
}

fn internal(e: impl std::fmt::Display) -> S3Error {
    tracing::error!(error = %e, "internal error");
    s3_error!(InternalError)
}

/// Maps storage errors to S3 errors. Errors raised by `s3s` while streaming the request body
/// (e.g. a payload hash mismatch) arrive wrapped in `io::Error` and are passed through as-is.
fn s3_err(e: StoreError) -> S3Error {
    match e {
        StoreError::NoSuchBucket(_) => s3_error!(NoSuchBucket),
        StoreError::NoSuchKey { .. } => s3_error!(NoSuchKey),
        StoreError::BucketAlreadyExists(_) => s3_error!(BucketAlreadyExists),
        StoreError::BucketAlreadyOwnedByYou(_) => s3_error!(BucketAlreadyOwnedByYou),
        StoreError::BucketNotEmpty(_) => s3_error!(BucketNotEmpty),
        StoreError::InvalidBucketName(_) => s3_error!(InvalidBucketName),
        StoreError::InvalidObjectKey => s3_error!(KeyTooLongError),
        StoreError::AccessDenied => s3_error!(AccessDenied),
        StoreError::UnknownStorageClass(_) => s3_error!(InvalidStorageClass),
        StoreError::BadDigest => s3_error!(BadDigest),
        StoreError::NoSuchUpload => s3_error!(NoSuchUpload),
        StoreError::InvalidPart(m) => S3Error::with_message(S3ErrorCode::InvalidPart, m),
        StoreError::InvalidPartOrder => s3_error!(InvalidPartOrder),
        StoreError::EntityTooSmall => s3_error!(EntityTooSmall),
        StoreError::PreconditionFailed => s3_error!(PreconditionFailed),
        StoreError::QuotaExceeded => {
            let mut e = S3Error::with_message(
                S3ErrorCode::Custom("QuotaExceeded".into()),
                "The storage quota of this account has been exceeded.",
            );
            e.set_status_code(axum::http::StatusCode::FORBIDDEN);
            e
        }
        StoreError::TooManyBuckets => s3_error!(TooManyBuckets),
        StoreError::MethodNotAllowed => s3_error!(MethodNotAllowed),
        StoreError::ObjectLocked => {
            s3_error!(
                AccessDenied,
                "Access Denied because object protected by object lock."
            )
        }
        StoreError::InvalidRequest(m) => S3Error::with_message(S3ErrorCode::InvalidRequest, m),
        e @ (StoreError::InvalidReplicaCount { .. } | StoreError::InvalidArgument(_)) => {
            S3Error::with_message(S3ErrorCode::InvalidArgument, e.to_string())
        }
        e @ (StoreError::WriteQuorum { .. }
        | StoreError::NoHealthyReplica(_)
        | StoreError::NoDisks) => {
            tracing::error!(error = %e, "storage unavailable");
            s3_error!(ServiceUnavailable)
        }
        StoreError::Io(io) => match io.into_inner() {
            Some(inner) => body_error(inner),
            None => internal("I/O error"),
        },
        other => internal(other),
    }
}

/// Classifies an error raised while reading the request body.
fn body_error(e: Box<dyn std::error::Error + Send + Sync>) -> S3Error {
    use s3s::stream::{
        aws_chunked_stream::AwsChunkedStreamError, upload_stream::UploadStreamError,
    };
    let e = match e.downcast::<S3Error>() {
        Ok(s3) => return *s3,
        Err(e) => e,
    };
    let e = match e.downcast::<UploadStreamError>() {
        Ok(u) => {
            return match *u {
                // AWS's code for a body that does not match the signed x-amz-content-sha256.
                UploadStreamError::Sha256Mismatch => {
                    let mut err = S3Error::with_message(
                        S3ErrorCode::Custom("XAmzContentSHA256Mismatch".into()),
                        "The provided 'x-amz-content-sha256' header does not match what was computed.",
                    );
                    err.set_status_code(axum::http::StatusCode::BAD_REQUEST);
                    err
                }
                UploadStreamError::Underlying(inner) => body_error(inner),
                other => S3Error::new(other.to_s3_error_code()),
            };
        }
        Err(e) => e,
    };
    let e = match e.downcast::<AwsChunkedStreamError>() {
        Ok(c) => {
            return match *c {
                AwsChunkedStreamError::Underlying(inner) => body_error(inner),
                other => S3Error::new(other.to_s3_error_code()),
            };
        }
        Err(e) => e,
    };
    if e.downcast_ref::<s3s::BodySizeLimitExceeded>().is_some() {
        return s3_error!(EntityTooLarge);
    }
    // Anything else here is the transport failing mid-upload (e.g. the client went away).
    tracing::warn!(error = %e, "request body could not be read");
    s3_error!(IncompleteBody)
}

fn timestamp(millis: i64) -> Timestamp {
    Timestamp::from(UNIX_EPOCH + Duration::from_millis(u64::try_from(millis).unwrap_or(0)))
}

fn etag(rec: &ObjectRecord) -> ETag {
    ETag::Strong(rec.etag.clone())
}

fn storage_class(rec: &ObjectRecord) -> ObjectStorageClass {
    ObjectStorageClass::from(
        rec.storage_class
            .clone()
            .unwrap_or_else(|| "STANDARD".to_owned()),
    )
}

fn user_metadata(rec: &ObjectRecord) -> Option<HashMap<String, String>> {
    (!rec.user_metadata.is_empty()).then(|| {
        rec.user_metadata
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    })
}

fn size_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Percent-encodes everything except RFC 3986 unreserved characters and `/`, which is what
/// `encoding-type=url` clients (e.g. the AWS CLI) decode.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn encoder(encoding_type: Option<&s3s::dto::EncodingType>) -> S3Result<fn(&str) -> String> {
    match encoding_type.map(s3s::dto::EncodingType::as_str) {
        None => Ok(str::to_owned),
        Some("url") => Ok(url_encode),
        Some(_) => Err(s3_error!(
            InvalidArgument,
            "Invalid Encoding Method specified"
        )),
    }
}

fn max_keys(v: Option<i32>) -> S3Result<usize> {
    match v {
        None => Ok(1000),
        Some(n) => usize::try_from(n).map_err(|_| s3_error!(InvalidArgument, "max-keys")),
    }
}

fn object_entries(page: &ListPage, enc: fn(&str) -> String) -> Vec<Object> {
    page.objects
        .iter()
        .map(|o| Object {
            key: Some(enc(&o.key)),
            e_tag: Some(etag(o)),
            last_modified: Some(timestamp(o.created_at)),
            size: Some(size_i64(o.size)),
            storage_class: Some(storage_class(o)),
            ..Default::default()
        })
        .collect()
}

fn prefix_entries(page: &ListPage, enc: fn(&str) -> String) -> Vec<CommonPrefix> {
    page.common_prefixes
        .iter()
        .map(|p| CommonPrefix {
            prefix: Some(enc(p)),
        })
        .collect()
}

fn parse_md5(content_md5: Option<&str>) -> S3Result<Option<[u8; 16]>> {
    content_md5
        .map(|b64| {
            B64.decode(b64)
                .ok()
                .and_then(|d| <[u8; 16]>::try_from(d).ok())
                .ok_or_else(|| s3_error!(InvalidDigest))
        })
        .transpose()
}

fn parse_upload_id(id: &str) -> S3Result<Uuid> {
    Uuid::parse_str(id).map_err(|_| s3_error!(NoSuchUpload))
}

fn part_number(n: i32) -> S3Result<u32> {
    u32::try_from(n).map_err(|_| s3_error!(InvalidArgument, "invalid part number"))
}

fn class_name(c: Option<StorageClass>) -> Option<String> {
    c.map(|c| c.as_str().to_owned())
}

fn copy_source(src: &CopySource) -> S3Result<(String, String)> {
    match src {
        CopySource::Bucket {
            bucket,
            key,
            version_id: None,
        } => Ok((bucket.to_string(), key.to_string())),
        CopySource::Bucket { .. } => Err(s3_error!(NotImplemented, "versioned copy sources")),
        _ => Err(s3_error!(NotImplemented, "access point copy sources")),
    }
}

/// Parses `x-amz-copy-source-range: bytes=first-last` into a half-open range.
fn copy_range(range: Option<&str>) -> S3Result<Option<std::ops::Range<u64>>> {
    let Some(range) = range else { return Ok(None) };
    let invalid = || s3_error!(InvalidArgument, "invalid x-amz-copy-source-range");
    let (first, last) = range
        .strip_prefix("bytes=")
        .and_then(|r| r.split_once('-'))
        .ok_or_else(invalid)?;
    let first: u64 = first.parse().map_err(|_| invalid())?;
    let last: u64 = last.parse().map_err(|_| invalid())?;
    if last < first {
        return Err(invalid());
    }
    Ok(Some(first..last + 1))
}

/// Evaluates conditional request headers as RFC 9110 and S3 do.
fn check_preconditions(
    rec: &ObjectRecord,
    if_match: Option<&ETagCondition>,
    if_none_match: Option<&ETagCondition>,
    if_modified_since: Option<&Timestamp>,
    if_unmodified_since: Option<&Timestamp>,
) -> S3Result<()> {
    let matches = |c: &ETagCondition| match c {
        ETagCondition::Any => true,
        ETagCondition::ETag(e) => e.value() == rec.etag,
    };
    let modified = rec.created_at.div_euclid(1000);
    let secs = |t: &Timestamp| time::OffsetDateTime::from(t.clone()).unix_timestamp();
    if let Some(c) = if_match {
        if !matches(c) {
            return Err(s3_error!(PreconditionFailed));
        }
    } else if let Some(t) = if_unmodified_since
        && modified > secs(t)
    {
        return Err(s3_error!(PreconditionFailed));
    }
    if let Some(c) = if_none_match {
        if matches(c) {
            return Err(s3_error!(NotModified));
        }
    } else if let Some(t) = if_modified_since
        && modified <= secs(t)
    {
        return Err(s3_error!(NotModified));
    }
    Ok(())
}

fn millis(t: Timestamp) -> i64 {
    i64::try_from(time::OffsetDateTime::from(t).unix_timestamp_nanos() / 1_000_000)
        .unwrap_or(i64::MAX)
}

/// The version id to report; the `null` version of unversioned buckets is not reported.
fn version_of(rec: &ObjectRecord) -> Option<String> {
    (rec.version_id != NULL_VERSION).then(|| rec.version_id.clone())
}

fn take_checksum(outcome: &checksum::Outcome) -> Checksum {
    outcome
        .lock()
        .expect("checksum lock")
        .take()
        .unwrap_or_default()
}

fn body_reader(body: Option<StreamingBlob>) -> Box<dyn AsyncRead + Unpin + Send> {
    match body {
        Some(blob) => Box::new(StreamReader::new(blob.map_err(io::Error::other))),
        None => Box::new(tokio::io::empty()),
    }
}

#[async_trait]
impl S3 for S3Service {
    async fn list_buckets(
        &self,
        req: S3Request<ListBucketsInput>,
    ) -> S3Result<S3Response<ListBucketsOutput>> {
        let tenant_id = self.tenant(&req, Class::A).await?;
        let tenant = self
            .store
            .metadata()
            .get_tenant(&tenant_id)
            .await
            .map_err(internal)?;
        let buckets = self
            .store
            .list_buckets(&tenant_id)
            .await
            .map_err(s3_err)?
            .into_iter()
            .map(|b| Bucket {
                name: Some(b.name),
                creation_date: Some(timestamp(b.created_at)),
                bucket_region: Some(self.region.clone()),
                ..Default::default()
            })
            .collect();
        Ok(S3Response::new(ListBucketsOutput {
            buckets: Some(buckets),
            owner: Some(Owner {
                id: Some(tenant_id),
                display_name: tenant.map(|t| t.name),
            }),
            ..Default::default()
        }))
    }

    async fn create_bucket(
        &self,
        req: S3Request<CreateBucketInput>,
    ) -> S3Result<S3Response<CreateBucketOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let name = req.input.bucket;
        self.store
            .create_bucket(&tenant, &name, None)
            .await
            .map_err(s3_err)?;
        if req.input.object_lock_enabled_for_bucket == Some(true) {
            self.store
                .enable_object_lock(&tenant, &name)
                .await
                .map_err(s3_err)?;
        }
        Ok(S3Response::new(CreateBucketOutput {
            location: Some(format!("/{name}")),
            ..Default::default()
        }))
    }

    async fn head_bucket(
        &self,
        req: S3Request<HeadBucketInput>,
    ) -> S3Result<S3Response<HeadBucketOutput>> {
        let tenant = self.tenant(&req, Class::B).await?;
        self.store
            .get_bucket(&tenant, &req.input.bucket)
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(HeadBucketOutput {
            bucket_region: Some(self.region.clone()),
            ..Default::default()
        }))
    }

    async fn get_bucket_location(
        &self,
        req: S3Request<GetBucketLocationInput>,
    ) -> S3Result<S3Response<GetBucketLocationOutput>> {
        let tenant = self.tenant(&req, Class::B).await?;
        self.store
            .get_bucket(&tenant, &req.input.bucket)
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(GetBucketLocationOutput {
            location_constraint: Some(BucketLocationConstraint::from(self.region.clone())),
        }))
    }

    async fn get_bucket_versioning(
        &self,
        req: S3Request<GetBucketVersioningInput>,
    ) -> S3Result<S3Response<GetBucketVersioningOutput>> {
        let tenant = self.tenant(&req, Class::B).await?;
        let b = self
            .store
            .get_bucket(&tenant, &req.input.bucket)
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(GetBucketVersioningOutput {
            status: b.versioning.map(BucketVersioningStatus::from),
            ..Default::default()
        }))
    }

    async fn put_bucket_versioning(
        &self,
        req: S3Request<PutBucketVersioningInput>,
    ) -> S3Result<S3Response<PutBucketVersioningOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        let status = input
            .versioning_configuration
            .status
            .ok_or_else(|| s3_error!(MalformedXML, "missing Status"))?;
        self.store
            .set_versioning(&tenant, &input.bucket, status.as_str())
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(PutBucketVersioningOutput::default()))
    }

    async fn list_object_versions(
        &self,
        req: S3Request<ListObjectVersionsInput>,
    ) -> S3Result<S3Response<ListObjectVersionsOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        let enc = encoder(input.encoding_type.as_ref())?;
        let prefix = input.prefix.clone().unwrap_or_default();
        let key_marker = input.key_marker.clone().unwrap_or_default();
        let page = self
            .store
            .list_object_versions(
                &tenant,
                &input.bucket,
                &prefix,
                &key_marker,
                input.version_id_marker.as_deref(),
                max_keys(input.max_keys)?,
            )
            .await
            .map_err(s3_err)?;
        let owner = Owner {
            id: Some(tenant.clone()),
            display_name: None,
        };
        let mut versions = Vec::new();
        let mut markers = Vec::new();
        for o in &page.versions {
            if o.delete_marker {
                markers.push(DeleteMarkerEntry {
                    key: Some(enc(&o.key)),
                    version_id: Some(o.version_id.clone()),
                    is_latest: Some(o.is_latest),
                    last_modified: Some(timestamp(o.created_at)),
                    owner: Some(owner.clone()),
                });
            } else {
                versions.push(ObjectVersion {
                    key: Some(enc(&o.key)),
                    version_id: Some(o.version_id.clone()),
                    is_latest: Some(o.is_latest),
                    last_modified: Some(timestamp(o.created_at)),
                    e_tag: Some(etag(o)),
                    size: Some(size_i64(o.size)),
                    storage_class: Some(ObjectVersionStorageClass::from(
                        o.storage_class
                            .clone()
                            .unwrap_or_else(|| "STANDARD".to_owned()),
                    )),
                    owner: Some(owner.clone()),
                    ..Default::default()
                });
            }
        }
        let last = page.versions.last();
        Ok(S3Response::new(ListObjectVersionsOutput {
            name: Some(input.bucket),
            prefix: Some(enc(&prefix)),
            key_marker: input.key_marker.as_deref().map(enc),
            version_id_marker: input.version_id_marker,
            max_keys: input.max_keys.or(Some(1000)),
            is_truncated: Some(page.truncated),
            next_key_marker: last.filter(|_| page.truncated).map(|o| enc(&o.key)),
            next_version_id_marker: last
                .filter(|_| page.truncated)
                .map(|o| o.version_id.clone()),
            versions: Some(versions),
            delete_markers: Some(markers),
            encoding_type: input.encoding_type,
            ..Default::default()
        }))
    }

    async fn put_bucket_lifecycle_configuration(
        &self,
        req: S3Request<PutBucketLifecycleConfigurationInput>,
    ) -> S3Result<S3Response<PutBucketLifecycleConfigurationOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        let rules = input
            .lifecycle_configuration
            .map(|c| c.rules)
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(i, r)| {
                if r.transitions.as_ref().is_some_and(|t| !t.is_empty()) {
                    return Err(s3_error!(NotImplemented, "lifecycle transitions"));
                }
                let prefix = r
                    .filter
                    .as_ref()
                    .and_then(|f| f.prefix.clone())
                    .or(r.prefix.clone())
                    .unwrap_or_default();
                let days = |d: Option<i32>| d.and_then(|d| u32::try_from(d).ok());
                Ok(LifecycleRule {
                    id: r.id.clone().unwrap_or_else(|| format!("rule-{}", i + 1)),
                    prefix,
                    enabled: r.status.as_str() == ExpirationStatus::ENABLED,
                    expiration_days: days(r.expiration.as_ref().and_then(|e| e.days)),
                    noncurrent_days: days(
                        r.noncurrent_version_expiration
                            .as_ref()
                            .and_then(|e| e.noncurrent_days),
                    ),
                    abort_multipart_days: days(
                        r.abort_incomplete_multipart_upload
                            .as_ref()
                            .and_then(|a| a.days_after_initiation),
                    ),
                })
            })
            .collect::<S3Result<Vec<_>>>()?;
        self.store
            .set_lifecycle(&tenant, &input.bucket, Some(&rules))
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(
            PutBucketLifecycleConfigurationOutput::default(),
        ))
    }

    async fn get_bucket_lifecycle_configuration(
        &self,
        req: S3Request<GetBucketLifecycleConfigurationInput>,
    ) -> S3Result<S3Response<GetBucketLifecycleConfigurationOutput>> {
        let tenant = self.tenant(&req, Class::B).await?;
        let rules = self
            .store
            .lifecycle(&tenant, &req.input.bucket)
            .await
            .map_err(s3_err)?
            .ok_or_else(|| s3_error!(NoSuchLifecycleConfiguration))?;
        let days = |d: Option<u32>| d.and_then(|d| i32::try_from(d).ok());
        let rules = rules
            .into_iter()
            .map(|r| S3LifecycleRule {
                id: Some(r.id),
                status: ExpirationStatus::from(
                    if r.enabled { "Enabled" } else { "Disabled" }.to_owned(),
                ),
                filter: Some(s3s::dto::LifecycleRuleFilter {
                    prefix: Some(r.prefix),
                    ..Default::default()
                }),
                expiration: r.expiration_days.map(|d| LifecycleExpiration {
                    days: days(Some(d)),
                    ..Default::default()
                }),
                noncurrent_version_expiration: r.noncurrent_days.map(|d| {
                    NoncurrentVersionExpiration {
                        noncurrent_days: days(Some(d)),
                        ..Default::default()
                    }
                }),
                abort_incomplete_multipart_upload: r.abort_multipart_days.map(|d| {
                    AbortIncompleteMultipartUpload {
                        days_after_initiation: days(Some(d)),
                    }
                }),
                noncurrent_version_transitions: None,
                prefix: None,
                transitions: None,
            })
            .collect();
        Ok(S3Response::new(GetBucketLifecycleConfigurationOutput {
            rules: Some(rules),
            ..Default::default()
        }))
    }

    async fn delete_bucket_lifecycle(
        &self,
        req: S3Request<DeleteBucketLifecycleInput>,
    ) -> S3Result<S3Response<DeleteBucketLifecycleOutput>> {
        let tenant = self.tenant(&req, Class::B).await?;
        self.store
            .set_lifecycle(&tenant, &req.input.bucket, None)
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(DeleteBucketLifecycleOutput::default()))
    }

    async fn put_object_lock_configuration(
        &self,
        req: S3Request<PutObjectLockConfigurationInput>,
    ) -> S3Result<S3Response<PutObjectLockConfigurationOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        let cfg = input.object_lock_configuration.unwrap_or_default();
        let current = self
            .store
            .object_lock_config(&tenant, &input.bucket)
            .await
            .map_err(s3_err)?;
        if !current.enabled {
            // S3 allows enabling Object Lock on an existing versioned bucket.
            if cfg
                .object_lock_enabled
                .as_ref()
                .map(ObjectLockEnabled::as_str)
                != Some(ObjectLockEnabled::ENABLED)
            {
                return Err(s3_error!(InvalidRequest, "Object Lock is not enabled"));
            }
            self.store
                .enable_object_lock(&tenant, &input.bucket)
                .await
                .map_err(s3_err)?;
        }
        let default = match cfg.rule.and_then(|r| r.default_retention) {
            None => None,
            Some(d) => {
                let mode = d
                    .mode
                    .ok_or_else(|| s3_error!(MalformedXML, "missing Mode"))?
                    .as_str()
                    .to_owned();
                let days = match (d.days, d.years) {
                    (Some(days), None) => days,
                    (None, Some(years)) => years.saturating_mul(365),
                    _ => return Err(s3_error!(MalformedXML, "specify Days or Years")),
                };
                let days = u32::try_from(days)
                    .ok()
                    .filter(|d| *d > 0)
                    .ok_or_else(|| s3_error!(InvalidArgument, "retention must be positive"))?;
                Some((mode, days))
            }
        };
        self.store
            .set_default_retention(&tenant, &input.bucket, default)
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(PutObjectLockConfigurationOutput::default()))
    }

    async fn get_object_lock_configuration(
        &self,
        req: S3Request<GetObjectLockConfigurationInput>,
    ) -> S3Result<S3Response<GetObjectLockConfigurationOutput>> {
        let tenant = self.tenant(&req, Class::B).await?;
        let cfg = self
            .store
            .object_lock_config(&tenant, &req.input.bucket)
            .await
            .map_err(s3_err)?;
        if !cfg.enabled {
            return Err(s3_error!(ObjectLockConfigurationNotFoundError));
        }
        let rule = match (cfg.default_mode, cfg.default_days) {
            (Some(mode), Some(days)) => Some(ObjectLockRule {
                default_retention: Some(DefaultRetention {
                    mode: Some(ObjectLockRetentionMode::from(mode)),
                    days: i32::try_from(days).ok(),
                    years: None,
                }),
            }),
            _ => None,
        };
        Ok(S3Response::new(GetObjectLockConfigurationOutput {
            object_lock_configuration: Some(ObjectLockConfiguration {
                object_lock_enabled: Some(ObjectLockEnabled::from_static(
                    ObjectLockEnabled::ENABLED,
                )),
                rule,
            }),
        }))
    }

    async fn put_object_retention(
        &self,
        req: S3Request<PutObjectRetentionInput>,
    ) -> S3Result<S3Response<PutObjectRetentionOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        let retention = match input.retention {
            Some(ObjectLockRetention {
                mode: Some(mode),
                retain_until_date: Some(until),
            }) => Some((mode.as_str().to_owned(), millis(until))),
            _ => None,
        };
        self.store
            .set_object_retention(
                &tenant,
                &input.bucket,
                &input.key,
                input.version_id.as_deref(),
                retention,
                input.bypass_governance_retention.unwrap_or(false),
            )
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(PutObjectRetentionOutput::default()))
    }

    async fn get_object_retention(
        &self,
        req: S3Request<GetObjectRetentionInput>,
    ) -> S3Result<S3Response<GetObjectRetentionOutput>> {
        let tenant = self.tenant(&req, Class::B).await?;
        let input = req.input;
        let o = self
            .store
            .object_lock_state(
                &tenant,
                &input.bucket,
                &input.key,
                input.version_id.as_deref(),
            )
            .await
            .map_err(s3_err)?;
        let (Some(mode), Some(until)) = (o.lock_mode, o.lock_until) else {
            return Err(s3_error!(NoSuchObjectLockConfiguration));
        };
        Ok(S3Response::new(GetObjectRetentionOutput {
            retention: Some(ObjectLockRetention {
                mode: Some(ObjectLockRetentionMode::from(mode)),
                retain_until_date: Some(timestamp(until)),
            }),
        }))
    }

    async fn put_object_legal_hold(
        &self,
        req: S3Request<PutObjectLegalHoldInput>,
    ) -> S3Result<S3Response<PutObjectLegalHoldOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        let on = input
            .legal_hold
            .and_then(|h| h.status)
            .is_some_and(|s| s.as_str() == ObjectLockLegalHoldStatus::ON);
        self.store
            .set_legal_hold(
                &tenant,
                &input.bucket,
                &input.key,
                input.version_id.as_deref(),
                on,
            )
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(PutObjectLegalHoldOutput::default()))
    }

    async fn get_object_legal_hold(
        &self,
        req: S3Request<GetObjectLegalHoldInput>,
    ) -> S3Result<S3Response<GetObjectLegalHoldOutput>> {
        let tenant = self.tenant(&req, Class::B).await?;
        let input = req.input;
        let o = self
            .store
            .object_lock_state(
                &tenant,
                &input.bucket,
                &input.key,
                input.version_id.as_deref(),
            )
            .await
            .map_err(s3_err)?;
        let status = if o.legal_hold {
            ObjectLockLegalHoldStatus::ON
        } else {
            ObjectLockLegalHoldStatus::OFF
        };
        Ok(S3Response::new(GetObjectLegalHoldOutput {
            legal_hold: Some(ObjectLockLegalHold {
                status: Some(ObjectLockLegalHoldStatus::from_static(status)),
            }),
        }))
    }

    async fn put_bucket_cors(
        &self,
        req: S3Request<PutBucketCorsInput>,
    ) -> S3Result<S3Response<PutBucketCorsOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        let rules: Vec<crate::cors::CorsRule> = input
            .cors_configuration
            .cors_rules
            .into_iter()
            .map(crate::cors::CorsRule::from)
            .collect();
        let json = serde_json::to_string(&rules).map_err(internal)?;
        self.store
            .set_cors(&tenant, &input.bucket, Some(&json))
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(PutBucketCorsOutput::default()))
    }

    async fn get_bucket_cors(
        &self,
        req: S3Request<GetBucketCorsInput>,
    ) -> S3Result<S3Response<GetBucketCorsOutput>> {
        let tenant = self.tenant(&req, Class::B).await?;
        let b = self
            .store
            .get_bucket(&tenant, &req.input.bucket)
            .await
            .map_err(s3_err)?;
        let rules = crate::cors::decode(b.cors.as_deref())
            .ok_or_else(|| s3_error!(NoSuchCORSConfiguration))?;
        Ok(S3Response::new(GetBucketCorsOutput {
            cors_rules: Some(rules.into_iter().map(CORSRule::from).collect()),
        }))
    }

    async fn delete_bucket_cors(
        &self,
        req: S3Request<DeleteBucketCorsInput>,
    ) -> S3Result<S3Response<DeleteBucketCorsOutput>> {
        let tenant = self.tenant(&req, Class::B).await?;
        self.store
            .set_cors(&tenant, &req.input.bucket, None)
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(DeleteBucketCorsOutput::default()))
    }

    async fn delete_bucket(
        &self,
        req: S3Request<DeleteBucketInput>,
    ) -> S3Result<S3Response<DeleteBucketOutput>> {
        let tenant = self.tenant(&req, Class::B).await?;
        self.store
            .delete_bucket(&tenant, &req.input.bucket)
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(DeleteBucketOutput {}))
    }

    async fn put_object(
        &self,
        req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let expected = checksum::expected(&req.headers, req.trailing_headers.as_ref())?;
        let input = req.input;
        let if_absent = match &input.if_none_match {
            None => false,
            Some(ETagCondition::Any) => true,
            Some(ETagCondition::ETag(_)) => {
                return Err(s3_error!(NotImplemented, "If-None-Match supports only *"));
            }
        };
        if input.if_match.is_some() {
            return Err(s3_error!(NotImplemented, "If-Match on PutObject"));
        }
        let request = PutObjectRequest {
            bucket: input.bucket,
            key: input.key,
            content_type: input.content_type,
            storage_class: class_name(input.storage_class),
            user_metadata: input
                .metadata
                .unwrap_or_default()
                .into_iter()
                .collect::<BTreeMap<_, _>>(),
            content_md5: parse_md5(input.content_md5.as_deref())?,
            if_absent,
            lock_mode: input.object_lock_mode.map(|m| m.as_str().to_owned()),
            lock_until: input.object_lock_retain_until_date.map(millis),
            legal_hold: input
                .object_lock_legal_hold_status
                .is_some_and(|s| s.as_str() == ObjectLockLegalHoldStatus::ON),
        };
        let (body, verified) = checksum::wrap(body_reader(input.body), expected);
        let rec = self
            .store
            .put_object(&tenant, request, body)
            .await
            .map_err(s3_err)?;
        self.tenancy.meter.ingress(&tenant, rec.size);
        let c = take_checksum(&verified);
        Ok(S3Response::new(PutObjectOutput {
            version_id: version_of(&rec),
            e_tag: Some(etag(&rec)),
            size: Some(size_i64(rec.size)),
            checksum_crc32: c.checksum_crc32,
            checksum_crc32c: c.checksum_crc32c,
            checksum_crc64nvme: c.checksum_crc64nvme,
            checksum_sha1: c.checksum_sha1,
            checksum_sha256: c.checksum_sha256,
            ..Default::default()
        }))
    }

    async fn copy_object(
        &self,
        req: S3Request<CopyObjectInput>,
    ) -> S3Result<S3Response<CopyObjectOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        let (src_bucket, src_key) = copy_source(&input.copy_source)?;
        let replace = match input.metadata_directive.as_ref().map(|d| d.as_str()) {
            None | Some("COPY") => false,
            Some("REPLACE") => true,
            Some(_) => return Err(s3_error!(InvalidArgument, "invalid metadata directive")),
        };
        let dest = PutObjectRequest {
            bucket: input.bucket,
            key: input.key,
            content_type: input.content_type,
            storage_class: class_name(input.storage_class),
            user_metadata: input.metadata.unwrap_or_default().into_iter().collect(),
            content_md5: None,
            if_absent: matches!(input.if_none_match, Some(ETagCondition::Any)),
            ..PutObjectRequest::default()
        };
        let rec = self
            .store
            .copy_object(&tenant, &src_bucket, &src_key, dest, replace)
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(CopyObjectOutput {
            copy_object_result: Some(CopyObjectResult {
                e_tag: Some(etag(&rec)),
                last_modified: Some(timestamp(rec.created_at)),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        let request = PutObjectRequest {
            bucket: input.bucket.clone(),
            key: input.key.clone(),
            content_type: input.content_type,
            storage_class: class_name(input.storage_class),
            user_metadata: input.metadata.unwrap_or_default().into_iter().collect(),
            content_md5: None,
            if_absent: false,
            ..PutObjectRequest::default()
        };
        let upload = self
            .store
            .create_multipart_upload(&tenant, request)
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(CreateMultipartUploadOutput {
            bucket: Some(input.bucket),
            key: Some(input.key),
            upload_id: Some(upload.id.to_string()),
            ..Default::default()
        }))
    }

    async fn upload_part(
        &self,
        req: S3Request<UploadPartInput>,
    ) -> S3Result<S3Response<UploadPartOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let expected = checksum::expected(&req.headers, req.trailing_headers.as_ref())?;
        let input = req.input;
        let (body, verified) = checksum::wrap(body_reader(input.body), expected);
        let part = self
            .store
            .upload_part(
                &tenant,
                &input.bucket,
                &input.key,
                parse_upload_id(&input.upload_id)?,
                part_number(input.part_number)?,
                parse_md5(input.content_md5.as_deref())?,
                body,
            )
            .await
            .map_err(s3_err)?;
        self.tenancy.meter.ingress(&tenant, part.size);
        let c = take_checksum(&verified);
        Ok(S3Response::new(UploadPartOutput {
            e_tag: Some(ETag::Strong(part.etag)),
            checksum_crc32: c.checksum_crc32,
            checksum_crc32c: c.checksum_crc32c,
            checksum_crc64nvme: c.checksum_crc64nvme,
            checksum_sha1: c.checksum_sha1,
            checksum_sha256: c.checksum_sha256,
            ..Default::default()
        }))
    }

    async fn upload_part_copy(
        &self,
        req: S3Request<UploadPartCopyInput>,
    ) -> S3Result<S3Response<UploadPartCopyOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        let (src_bucket, src_key) = copy_source(&input.copy_source)?;
        let part = self
            .store
            .upload_part_copy(
                &tenant,
                &input.bucket,
                &input.key,
                parse_upload_id(&input.upload_id)?,
                part_number(input.part_number)?,
                &src_bucket,
                &src_key,
                copy_range(input.copy_source_range.as_deref())?,
            )
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(UploadPartCopyOutput {
            copy_part_result: Some(CopyPartResult {
                e_tag: Some(ETag::Strong(part.etag)),
                last_modified: Some(timestamp(part.created_at)),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    async fn complete_multipart_upload(
        &self,
        req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        let upload_id = parse_upload_id(&input.upload_id)?;
        let parts = input
            .multipart_upload
            .and_then(|m| m.parts)
            .unwrap_or_default()
            .into_iter()
            .map(|p| {
                Ok(CompletedPart {
                    part_number: part_number(
                        p.part_number.ok_or_else(|| s3_error!(MalformedXML))?,
                    )?,
                    etag: p.e_tag.ok_or_else(|| s3_error!(MalformedXML))?.into_value(),
                })
            })
            .collect::<S3Result<Vec<_>>>()?;
        if parts.is_empty() {
            return Err(s3_error!(MalformedXML, "no parts specified"));
        }
        // Assembling large objects takes a while; s3s keeps the connection alive meanwhile.
        let store = Arc::clone(&self.store);
        let (bucket, key) = (input.bucket, input.key);
        let future = async move {
            let rec = store
                .complete_multipart_upload(&tenant, &bucket, &key, upload_id, &parts)
                .await
                .map_err(s3_err)?;
            Ok(CompleteMultipartUploadOutput {
                location: Some(format!("/{bucket}/{key}")),
                bucket: Some(bucket),
                key: Some(key),
                e_tag: Some(etag(&rec)),
                version_id: version_of(&rec),
                ..Default::default()
            })
        };
        Ok(S3Response::new(CompleteMultipartUploadOutput {
            future: Some(Box::pin(future)),
            ..Default::default()
        }))
    }

    async fn abort_multipart_upload(
        &self,
        req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        let tenant = self.tenant(&req, Class::B).await?;
        let input = req.input;
        self.store
            .abort_multipart_upload(
                &tenant,
                &input.bucket,
                &input.key,
                parse_upload_id(&input.upload_id)?,
            )
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(AbortMultipartUploadOutput::default()))
    }

    async fn list_parts(
        &self,
        req: S3Request<ListPartsInput>,
    ) -> S3Result<S3Response<ListPartsOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        let marker = part_number(input.part_number_marker.unwrap_or(0))?;
        let max = max_keys(input.max_parts)?;
        let (upload, page) = self
            .store
            .list_parts(
                &tenant,
                &input.bucket,
                &input.key,
                parse_upload_id(&input.upload_id)?,
                marker,
                max,
            )
            .await
            .map_err(s3_err)?;
        let owner = Owner {
            id: Some(tenant.clone()),
            display_name: None,
        };
        Ok(S3Response::new(ListPartsOutput {
            bucket: Some(input.bucket),
            key: Some(input.key),
            upload_id: Some(input.upload_id),
            part_number_marker: input.part_number_marker,
            next_part_number_marker: page
                .parts
                .last()
                .and_then(|p| i32::try_from(p.part_number).ok()),
            max_parts: input.max_parts.or(Some(1000)),
            is_truncated: Some(page.truncated),
            storage_class: Some(StorageClass::from(
                upload
                    .storage_class
                    .unwrap_or_else(|| "STANDARD".to_owned()),
            )),
            initiator: Some(Initiator {
                id: Some(tenant),
                display_name: None,
            }),
            owner: Some(owner),
            parts: Some(
                page.parts
                    .into_iter()
                    .map(|p| Part {
                        part_number: i32::try_from(p.part_number).ok(),
                        e_tag: Some(ETag::Strong(p.etag)),
                        size: Some(size_i64(p.size)),
                        last_modified: Some(timestamp(p.created_at)),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }))
    }

    async fn list_multipart_uploads(
        &self,
        req: S3Request<ListMultipartUploadsInput>,
    ) -> S3Result<S3Response<ListMultipartUploadsOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        let enc = encoder(input.encoding_type.as_ref())?;
        let page = self
            .store
            .list_multipart_uploads(
                &tenant,
                &input.bucket,
                input.prefix.as_deref().unwrap_or_default(),
                input.key_marker.as_deref().unwrap_or_default(),
                input.upload_id_marker.as_deref().unwrap_or_default(),
                max_keys(input.max_uploads)?,
            )
            .await
            .map_err(s3_err)?;
        let last = page
            .uploads
            .last()
            .map(|u| (u.key.clone(), u.id.to_string()));
        Ok(S3Response::new(ListMultipartUploadsOutput {
            bucket: Some(input.bucket),
            prefix: input.prefix.as_deref().map(enc),
            key_marker: input.key_marker.as_deref().map(enc),
            upload_id_marker: input.upload_id_marker,
            max_uploads: input.max_uploads.or(Some(1000)),
            is_truncated: Some(page.truncated),
            next_key_marker: last.as_ref().map(|(k, _)| enc(k)),
            next_upload_id_marker: last.map(|(_, id)| id),
            encoding_type: input.encoding_type,
            uploads: Some(
                page.uploads
                    .into_iter()
                    .map(|u| MultipartUpload {
                        key: Some(enc(&u.key)),
                        upload_id: Some(u.id.to_string()),
                        initiated: Some(timestamp(u.created_at)),
                        storage_class: Some(StorageClass::from(
                            u.storage_class.unwrap_or_else(|| "STANDARD".to_owned()),
                        )),
                        owner: Some(Owner {
                            id: Some(tenant.clone()),
                            display_name: None,
                        }),
                        initiator: Some(Initiator {
                            id: Some(tenant.clone()),
                            display_name: None,
                        }),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }))
    }

    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let reader = self.reader(&req).await?;
        let input = req.input;
        let (stored, payer) = match (&input.version_id, &reader) {
            (Some(v), Some(tenant)) => (
                self.store
                    .get_object_version(tenant, &input.bucket, &input.key, v)
                    .await
                    .map_err(s3_err)?,
                tenant.clone(),
            ),
            (Some(_), None) => return Err(s3_error!(AccessDenied)),
            (None, _) => {
                self.lookup_for_read(reader.as_deref(), &input.bucket, &input.key)
                    .await?
            }
        };
        let rec = &stored.object;
        check_preconditions(
            rec,
            input.if_match.as_ref(),
            input.if_none_match.as_ref(),
            input.if_modified_since.as_ref(),
            input.if_unmodified_since.as_ref(),
        )?;

        let (start, len, content_range) = match &input.range {
            None => (0, rec.size, None),
            Some(range) => {
                let r = range.check(rec.size).map_err(|_| s3_error!(InvalidRange))?;
                let range_header = format!("bytes {}-{}/{}", r.start, r.end - 1, rec.size);
                (r.start, r.end - r.start, Some(range_header))
            }
        };
        let reader = self
            .store
            .read_object(&stored, start..start + len)
            .await
            .map_err(s3_err)?;
        self.tenancy.meter.egress(&payer, len);
        let body = ReaderStream::with_capacity(reader, READ_CHUNK);

        Ok(S3Response::new(GetObjectOutput {
            body: Some(StreamingBlob::wrap(body)),
            content_length: Some(size_i64(len)),
            content_range,
            version_id: version_of(rec),
            accept_ranges: Some("bytes".to_owned()),
            e_tag: Some(etag(rec)),
            last_modified: Some(timestamp(rec.created_at)),
            content_type: rec.content_type.clone(),
            metadata: user_metadata(rec),
            ..Default::default()
        }))
    }

    async fn head_object(
        &self,
        req: S3Request<HeadObjectInput>,
    ) -> S3Result<S3Response<HeadObjectOutput>> {
        let reader = self.reader(&req).await?;
        let input = &req.input;
        let (stored, _) = match (&input.version_id, &reader) {
            (Some(v), Some(tenant)) => (
                self.store
                    .get_object_version(tenant, &input.bucket, &input.key, v)
                    .await
                    .map_err(s3_err)?,
                tenant.clone(),
            ),
            (Some(_), None) => return Err(s3_error!(AccessDenied)),
            (None, _) => {
                self.lookup_for_read(reader.as_deref(), &input.bucket, &input.key)
                    .await?
            }
        };
        let rec = stored.object;
        check_preconditions(
            &rec,
            input.if_match.as_ref(),
            input.if_none_match.as_ref(),
            input.if_modified_since.as_ref(),
            input.if_unmodified_since.as_ref(),
        )?;
        Ok(S3Response::new(HeadObjectOutput {
            version_id: version_of(&rec),
            content_length: Some(size_i64(rec.size)),
            accept_ranges: Some("bytes".to_owned()),
            e_tag: Some(etag(&rec)),
            last_modified: Some(timestamp(rec.created_at)),
            content_type: rec.content_type.clone(),
            metadata: user_metadata(&rec),
            ..Default::default()
        }))
    }

    async fn delete_object(
        &self,
        req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        let tenant = self.tenant(&req, Class::B).await?;
        let input = req.input;
        let result = match &input.version_id {
            Some(v) => {
                self.store
                    .delete_object_version(
                        &tenant,
                        &input.bucket,
                        &input.key,
                        v,
                        input.bypass_governance_retention.unwrap_or(false),
                    )
                    .await
            }
            None => {
                self.store
                    .delete_object(&tenant, &input.bucket, &input.key)
                    .await
            }
        }
        .map_err(s3_err)?;
        Ok(S3Response::new(DeleteObjectOutput {
            delete_marker: result.delete_marker.then_some(true),
            version_id: result.version_id.filter(|v| v != NULL_VERSION),
            ..Default::default()
        }))
    }

    async fn delete_objects(
        &self,
        req: S3Request<DeleteObjectsInput>,
    ) -> S3Result<S3Response<DeleteObjectsOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        if input.delete.objects.len() > 1000 {
            return Err(s3_error!(MalformedXML, "at most 1000 keys per request"));
        }
        let quiet = input.delete.quiet.unwrap_or(false);
        let mut deleted = Vec::new();
        let mut errors = Vec::new();
        for obj in input.delete.objects {
            let result = match &obj.version_id {
                Some(v) => {
                    self.store
                        .delete_object_version(
                            &tenant,
                            &input.bucket,
                            &obj.key,
                            v,
                            input.bypass_governance_retention.unwrap_or(false),
                        )
                        .await
                }
                None => {
                    self.store
                        .delete_object(&tenant, &input.bucket, &obj.key)
                        .await
                }
            };
            match result {
                Ok(r) => {
                    if !quiet {
                        let marker_version = r
                            .delete_marker
                            .then(|| r.version_id.clone())
                            .flatten()
                            .filter(|_| obj.version_id.is_none());
                        deleted.push(DeletedObject {
                            key: Some(obj.key),
                            version_id: obj.version_id.clone(),
                            delete_marker: r.delete_marker.then_some(true),
                            delete_marker_version_id: marker_version,
                        });
                    }
                }
                Err(e) => {
                    let err = s3_err(e);
                    errors.push(s3s::dto::Error {
                        code: Some(err.code().as_str().to_owned()),
                        key: Some(obj.key),
                        message: err.message().map(str::to_owned),
                        version_id: None,
                    });
                }
            }
        }
        Ok(S3Response::new(DeleteObjectsOutput {
            deleted: Some(deleted),
            errors: Some(errors),
            ..Default::default()
        }))
    }

    async fn list_objects_v2(
        &self,
        req: S3Request<ListObjectsV2Input>,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        let enc = encoder(input.encoding_type.as_ref())?;
        let start = if let Some(token) = &input.continuation_token {
            let cursor = B64URL
                .decode(token)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .ok_or_else(|| s3_error!(InvalidArgument, "invalid continuation token"))?;
            ListStart::Cursor(cursor)
        } else if let Some(after) = &input.start_after {
            ListStart::Marker(after.clone())
        } else {
            ListStart::Beginning
        };
        let max = max_keys(input.max_keys)?;
        let page = self
            .store
            .list_objects(
                &tenant,
                &ListParams {
                    bucket: input.bucket.clone(),
                    prefix: input.prefix.clone().unwrap_or_default(),
                    delimiter: input.delimiter.clone(),
                    start,
                    max_keys: max,
                },
            )
            .await
            .map_err(s3_err)?;
        let key_count = page.objects.len() + page.common_prefixes.len();
        Ok(S3Response::new(ListObjectsV2Output {
            name: Some(input.bucket),
            prefix: Some(enc(input.prefix.as_deref().unwrap_or_default())),
            delimiter: input.delimiter.as_deref().map(enc),
            start_after: input.start_after.as_deref().map(enc),
            max_keys: input.max_keys.or(Some(1000)),
            key_count: Some(i32::try_from(key_count).unwrap_or(i32::MAX)),
            is_truncated: Some(page.truncated),
            continuation_token: input.continuation_token,
            next_continuation_token: page.next_cursor.as_deref().map(|c| B64URL.encode(c)),
            contents: Some(object_entries(&page, enc)),
            common_prefixes: Some(prefix_entries(&page, enc)),
            encoding_type: input.encoding_type,
            ..Default::default()
        }))
    }

    async fn list_objects(
        &self,
        req: S3Request<ListObjectsInput>,
    ) -> S3Result<S3Response<ListObjectsOutput>> {
        let tenant = self.tenant(&req, Class::A).await?;
        let input = req.input;
        let enc = encoder(input.encoding_type.as_ref())?;
        let start = match &input.marker {
            Some(m) if !m.is_empty() => ListStart::Marker(m.clone()),
            _ => ListStart::Beginning,
        };
        let page = self
            .store
            .list_objects(
                &tenant,
                &ListParams {
                    bucket: input.bucket.clone(),
                    prefix: input.prefix.clone().unwrap_or_default(),
                    delimiter: input.delimiter.clone(),
                    start,
                    max_keys: max_keys(input.max_keys)?,
                },
            )
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(ListObjectsOutput {
            name: Some(input.bucket),
            prefix: Some(enc(input.prefix.as_deref().unwrap_or_default())),
            delimiter: input.delimiter.as_deref().map(enc),
            marker: Some(enc(input.marker.as_deref().unwrap_or_default())),
            max_keys: input.max_keys.or(Some(1000)),
            is_truncated: Some(page.truncated),
            next_marker: if page.truncated {
                page.next_marker.as_deref().map(enc)
            } else {
                None
            },
            contents: Some(object_entries(&page, enc)),
            common_prefixes: Some(prefix_entries(&page, enc)),
            encoding_type: input.encoding_type,
            ..Default::default()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::url_encode;

    #[test]
    fn url_encoding_keeps_slashes_and_unreserved() {
        assert_eq!(url_encode("a/b-c_d.e~f"), "a/b-c_d.e~f");
        assert_eq!(url_encode("a b+c%"), "a%20b%2Bc%25");
        assert_eq!(url_encode("日"), "%E6%97%A5");
    }
}
