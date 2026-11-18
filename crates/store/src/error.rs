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

    /// The key contains a character that XML 1.0 cannot carry (a control character other than
    /// tab / LF / CR), so it could never be returned by a listing without `encoding-type=url`.
    #[error("object key contains a control character")]
    InvalidKeyCharacter,

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

    #[error("invalid tag: {0}")]
    InvalidTag(String),

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

    /// A cluster-wide maintenance job is already running elsewhere (its lease is held).
    #[error("{0}")]
    Busy(String),

    #[error("only {written} of the required {required} replicas could be written")]
    WriteQuorum { required: usize, written: usize },

    #[error("no healthy replica of object {0} is available")]
    NoHealthyReplica(Uuid),

    #[error("metadata store error")]
    Metadata(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error(transparent)]
    Io(#[from] io::Error),
}

impl StoreError {
    /// Whether the metadata database could not be reached (connection refused or lost, or no
    /// pooled connection became free in time), as opposed to a query that failed.
    pub fn is_metadata_unavailable(&self) -> bool {
        let Self::Metadata(inner) = self else {
            return false;
        };
        matches!(
            inner.downcast_ref::<sqlx::Error>(),
            Some(
                sqlx::Error::PoolTimedOut
                    | sqlx::Error::PoolClosed
                    | sqlx::Error::Io(_)
                    | sqlx::Error::Tls(_)
                    | sqlx::Error::Protocol(_)
            )
        )
    }
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        Self::Metadata(Box::new(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_failures_count_as_metadata_unavailable() {
        assert!(StoreError::from(sqlx::Error::PoolTimedOut).is_metadata_unavailable());
        let refused = io::Error::new(io::ErrorKind::ConnectionRefused, "refused");
        assert!(StoreError::from(sqlx::Error::Io(refused)).is_metadata_unavailable());
        assert!(!StoreError::from(sqlx::Error::RowNotFound).is_metadata_unavailable());
        assert!(!StoreError::NoDisks.is_metadata_unavailable());
    }
}
