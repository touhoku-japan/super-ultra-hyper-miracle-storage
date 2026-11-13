//! The S3 API, implemented on top of the storage engine via `s3s`.
//!
//! `s3s` parses requests, verifies SigV4 signatures (including aws-chunked payloads and
//! `x-amz-content-sha256`) and renders XML. Every operation here first maps the verified access
//! key to its tenant; the store then enforces that tenants only see their own buckets.

use std::{
    collections::{BTreeMap, HashMap},
    io::{self, SeekFrom},
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
        Bucket, BucketLocationConstraint, CommonPrefix, CreateBucketInput, CreateBucketOutput,
        DeleteBucketInput, DeleteBucketOutput, DeleteObjectInput, DeleteObjectOutput,
        DeleteObjectsInput, DeleteObjectsOutput, DeletedObject, ETag, GetBucketLocationInput,
        GetBucketLocationOutput, GetBucketVersioningInput, GetBucketVersioningOutput,
        GetObjectInput, GetObjectOutput, HeadBucketInput, HeadBucketOutput, HeadObjectInput,
        HeadObjectOutput, ListBucketsInput, ListBucketsOutput, ListObjectsInput, ListObjectsOutput,
        ListObjectsV2Input, ListObjectsV2Output, Object, ObjectStorageClass, Owner, PutObjectInput,
        PutObjectOutput, StreamingBlob, Timestamp,
    },
    s3_error,
};
use suhms_store::{
    ListPage, ListParams, ListStart, ObjectRecord, PutObjectRequest, Store, StoreError,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};
use tokio_util::io::{ReaderStream, StreamReader};

use crate::crypto::SecretCipher;

const READ_CHUNK: usize = 256 * 1024;

/// Resolves access keys for SigV4 verification.
pub struct KeyStore {
    store: Arc<Store>,
    cipher: Arc<SecretCipher>,
}

impl KeyStore {
    pub fn new(store: Arc<Store>, cipher: Arc<SecretCipher>) -> Self {
        Self { store, cipher }
    }
}

#[async_trait]
impl S3Auth for KeyStore {
    async fn get_secret_key(&self, access_key: &str) -> S3Result<SecretKey> {
        let key = self
            .store
            .metadata()
            .get_access_key(access_key)
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
    region: String,
}

impl S3Service {
    pub fn new(store: Arc<Store>, region: String) -> Self {
        Self { store, region }
    }

    /// Tenant id of the (already signature-verified) caller.
    async fn tenant<T>(&self, req: &S3Request<T>) -> S3Result<String> {
        let cred = req
            .credentials
            .as_ref()
            .ok_or_else(|| s3_error!(AccessDenied, "Signature is required"))?;
        let key = self
            .store
            .metadata()
            .get_access_key(&cred.access_key)
            .await
            .map_err(internal)?
            .filter(|k| k.active)
            .ok_or_else(|| s3_error!(InvalidAccessKeyId))?;
        Ok(key.tenant_id)
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
        let tenant_id = self.tenant(&req).await?;
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
        let tenant = self.tenant(&req).await?;
        let name = req.input.bucket;
        self.store
            .create_bucket(&tenant, &name, None)
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(CreateBucketOutput {
            location: Some(format!("/{name}")),
            ..Default::default()
        }))
    }

    async fn head_bucket(
        &self,
        req: S3Request<HeadBucketInput>,
    ) -> S3Result<S3Response<HeadBucketOutput>> {
        let tenant = self.tenant(&req).await?;
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
        let tenant = self.tenant(&req).await?;
        self.store
            .get_bucket(&tenant, &req.input.bucket)
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(GetBucketLocationOutput {
            location_constraint: Some(BucketLocationConstraint::from(self.region.clone())),
        }))
    }

    /// Versioning is not supported yet; report it as never enabled.
    async fn get_bucket_versioning(
        &self,
        req: S3Request<GetBucketVersioningInput>,
    ) -> S3Result<S3Response<GetBucketVersioningOutput>> {
        let tenant = self.tenant(&req).await?;
        self.store
            .get_bucket(&tenant, &req.input.bucket)
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(GetBucketVersioningOutput::default()))
    }

    async fn delete_bucket(
        &self,
        req: S3Request<DeleteBucketInput>,
    ) -> S3Result<S3Response<DeleteBucketOutput>> {
        let tenant = self.tenant(&req).await?;
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
        let tenant = self.tenant(&req).await?;
        let input = req.input;
        let content_md5 = input
            .content_md5
            .as_deref()
            .map(|b64| {
                B64.decode(b64)
                    .ok()
                    .and_then(|d| <[u8; 16]>::try_from(d).ok())
                    .ok_or_else(|| s3_error!(InvalidDigest))
            })
            .transpose()?;
        let request = PutObjectRequest {
            bucket: input.bucket,
            key: input.key,
            content_type: input.content_type,
            storage_class: input.storage_class.map(|c| c.as_str().to_owned()),
            user_metadata: input
                .metadata
                .unwrap_or_default()
                .into_iter()
                .collect::<BTreeMap<_, _>>(),
            content_md5,
        };
        let rec = self
            .store
            .put_object(&tenant, request, body_reader(input.body))
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(PutObjectOutput {
            e_tag: Some(etag(&rec)),
            size: Some(size_i64(rec.size)),
            ..Default::default()
        }))
    }

    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let tenant = self.tenant(&req).await?;
        let input = req.input;
        let (rec, mut file) = self
            .store
            .get_object(&tenant, &input.bucket, &input.key)
            .await
            .map_err(s3_err)?;

        let (start, len, content_range) = match &input.range {
            None => (0, rec.size, None),
            Some(range) => {
                let r = range.check(rec.size).map_err(|_| s3_error!(InvalidRange))?;
                let range_header = format!("bytes {}-{}/{}", r.start, r.end - 1, rec.size);
                (r.start, r.end - r.start, Some(range_header))
            }
        };
        if start > 0 {
            file.seek(SeekFrom::Start(start)).await.map_err(internal)?;
        }
        let body = ReaderStream::with_capacity(file.take(len), READ_CHUNK);

        Ok(S3Response::new(GetObjectOutput {
            body: Some(StreamingBlob::wrap(body)),
            content_length: Some(size_i64(len)),
            content_range,
            accept_ranges: Some("bytes".to_owned()),
            e_tag: Some(etag(&rec)),
            last_modified: Some(timestamp(rec.created_at)),
            content_type: rec.content_type.clone(),
            metadata: user_metadata(&rec),
            ..Default::default()
        }))
    }

    async fn head_object(
        &self,
        req: S3Request<HeadObjectInput>,
    ) -> S3Result<S3Response<HeadObjectOutput>> {
        let tenant = self.tenant(&req).await?;
        let rec = self
            .store
            .head_object(&tenant, &req.input.bucket, &req.input.key)
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(HeadObjectOutput {
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
        let tenant = self.tenant(&req).await?;
        self.store
            .delete_object(&tenant, &req.input.bucket, &req.input.key)
            .await
            .map_err(s3_err)?;
        Ok(S3Response::new(DeleteObjectOutput::default()))
    }

    async fn delete_objects(
        &self,
        req: S3Request<DeleteObjectsInput>,
    ) -> S3Result<S3Response<DeleteObjectsOutput>> {
        let tenant = self.tenant(&req).await?;
        let input = req.input;
        if input.delete.objects.len() > 1000 {
            return Err(s3_error!(MalformedXML, "at most 1000 keys per request"));
        }
        let quiet = input.delete.quiet.unwrap_or(false);
        let mut deleted = Vec::new();
        let mut errors = Vec::new();
        for obj in input.delete.objects {
            match self
                .store
                .delete_object(&tenant, &input.bucket, &obj.key)
                .await
            {
                Ok(_) => {
                    if !quiet {
                        deleted.push(DeletedObject {
                            key: Some(obj.key),
                            ..Default::default()
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
        let tenant = self.tenant(&req).await?;
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
        let tenant = self.tenant(&req).await?;
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
