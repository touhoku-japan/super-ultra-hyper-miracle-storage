use std::{io, path::PathBuf};

use uuid::Uuid;

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid configuration: {0}")]
    Config(String),

    #[error("disk {} is unavailable", path.display())]
    DiskUnavailable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("disk {} has an invalid format: {reason}", path.display())]
    DiskFormat { path: PathBuf, reason: String },

    #[error("no usable disks")]
    NoDisks,

    #[error("invalid bucket name: {0}")]
    InvalidBucketName(String),

    #[error("invalid object key")]
    InvalidObjectKey,

    #[error("bucket {0} does not exist")]
    NoSuchBucket(String),

    #[error("bucket {0} already exists")]
    BucketAlreadyExists(String),

    #[error("bucket {0} is already owned by you")]
    BucketAlreadyOwnedByYou(String),

    #[error("access denied")]
    AccessDenied,

    #[error("tenant {0} does not exist")]
    NoSuchTenant(String),

    #[error("tenant {0} already exists")]
    TenantAlreadyExists(String),

    #[error("the uploaded content does not match the supplied digest")]
    BadDigest,

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("multipart upload does not exist")]
    NoSuchUpload,

    #[error("invalid part: {0}")]
    InvalidPart(String),

    #[error("parts must be listed in ascending order")]
    InvalidPartOrder,

    #[error("a part other than the last is smaller than the minimum part size")]
    EntityTooSmall,

    #[error("precondition failed")]
    PreconditionFailed,

    #[error("the specified method is not allowed against this resource")]
    MethodNotAllowed,

    #[error("the object is locked (Object Lock retention or legal hold)")]
    ObjectLocked,

    #[error("storage quota exceeded")]
    QuotaExceeded,

    #[error("bucket limit reached")]
    TooManyBuckets,

    #[error("bucket {0} is not empty")]
    BucketNotEmpty(String),

    #[error("object {bucket}/{key} does not exist")]
    NoSuchKey { bucket: String, key: String },

    #[error("unknown storage class {0}")]
    UnknownStorageClass(String),

    #[error("replica count {requested} is invalid (allowed: 1..={max})")]
    InvalidReplicaCount { requested: u8, max: u8 },

    #[error("only {written} of the required {required} replicas could be written")]
    WriteQuorum { required: usize, written: usize },

    #[error("no healthy replica of object {0} is available")]
    NoHealthyReplica(Uuid),

    #[error("metadata store error")]
    Metadata(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        Self::Metadata(Box::new(e))
    }
}
