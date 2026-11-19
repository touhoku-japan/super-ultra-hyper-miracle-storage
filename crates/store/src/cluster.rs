//! Disks on other nodes.
//!
//! The storage engine reaches a remote disk through a [`Peer`] (implemented by the server over
//! its internal HTTP API, or in-process by tests). [`DiskRef`] hides whether a disk is local.

use std::{io, ops::Range, sync::Arc};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::AsyncRead;
use uuid::Uuid;

use crate::{
    blob::{self, Dest, LocalSink},
    disk::{Disk, DiskId},
};

pub type BoxReader = Box<dyn AsyncRead + Send + Sync + Unpin>;

/// A write in progress on one disk. Nothing becomes visible until [`ReplicaSink::commit`].
#[async_trait]
pub trait ReplicaSink: Send {
    async fn write(&mut self, chunk: Bytes) -> io::Result<()>;
    /// Makes the data durable (fsync) and visible under its final name.
    async fn commit(self: Box<Self>) -> io::Result<()>;
    /// Discards the write.
    async fn abort(self: Box<Self>);
}

/// Disk operations on another node.
#[async_trait]
pub trait Peer: Send + Sync {
    async fn open_sink(&self, disk: DiskId, dest: Dest) -> io::Result<Box<dyn ReplicaSink>>;
    /// Reads `range` of an object file whose full size must be `size`.
    async fn read(
        &self,
        disk: DiskId,
        blob: Uuid,
        size: u64,
        range: Range<u64>,
    ) -> io::Result<BoxReader>;
    /// Reads a whole part file of a multipart upload, whose size must be `size`. Used to copy a
    /// part to a disk that missed it, so that the upload can still be completed there.
    async fn read_part(
        &self,
        disk: DiskId,
        upload: Uuid,
        blob: Uuid,
        size: u64,
    ) -> io::Result<BoxReader> {
        let _ = (disk, upload, blob, size);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "reading parts from this node is not supported",
        ))
    }
    async fn remove(&self, disk: DiskId, dest: Dest) -> io::Result<()>;
    async fn remove_upload(&self, disk: DiskId, upload: Uuid) -> io::Result<()>;
    /// Concatenates parts into an object file on that disk; returns the object's BLAKE3.
    async fn concat(
        &self,
        disk: DiskId,
        upload: Uuid,
        parts: Vec<blob::JoinPart>,
        object: Uuid,
    ) -> io::Result<String>;
    /// Checks that the node answers (with a short timeout).
    async fn ping(&self) -> io::Result<()> {
        Ok(())
    }
}

/// Looks up the peer serving a node.
pub trait Peers: Send + Sync {
    fn get(&self, node_id: &str) -> Option<Arc<dyn Peer>>;
}

/// Single-node deployments: there are no other nodes.
pub struct NoPeers;

impl Peers for NoPeers {
    fn get(&self, _node_id: &str) -> Option<Arc<dyn Peer>> {
        None
    }
}

/// A disk on this node or on another one.
#[derive(Clone)]
pub enum DiskRef {
    Local(Arc<Disk>),
    Remote {
        id: DiskId,
        node_id: String,
        peer: Arc<dyn Peer>,
    },
}

impl std::fmt::Debug for DiskRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

impl DiskRef {
    pub fn id(&self) -> DiskId {
        match self {
            Self::Local(d) => d.id,
            Self::Remote { id, .. } => *id,
        }
    }

    pub fn node_id(&self) -> &str {
        match self {
            Self::Local(d) => &d.node_id,
            Self::Remote { node_id, .. } => node_id,
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// For logs.
    pub fn label(&self) -> String {
        match self {
            Self::Local(d) => d.root.display().to_string(),
            Self::Remote { id, node_id, .. } => format!("{node_id}:{id}"),
        }
    }

    pub async fn open_sink(&self, dest: Dest) -> io::Result<Box<dyn ReplicaSink>> {
        match self {
            Self::Local(d) => Ok(Box::new(LocalSink::create(d, dest).await?)),
            Self::Remote { id, peer, .. } => peer.open_sink(*id, dest).await,
        }
    }

    pub async fn read(&self, blob: Uuid, size: u64, range: Range<u64>) -> io::Result<BoxReader> {
        match self {
            Self::Local(d) => blob::read_local(d, blob, size, range).await,
            Self::Remote { id, peer, .. } => peer.read(*id, blob, size, range).await,
        }
    }

    pub async fn read_part(&self, upload: Uuid, blob: Uuid, size: u64) -> io::Result<BoxReader> {
        match self {
            Self::Local(d) => {
                blob::read_local_path(&d.part_path(upload, blob), size, 0..size).await
            }
            Self::Remote { id, peer, .. } => peer.read_part(*id, upload, blob, size).await,
        }
    }

    /// Best effort: failures are logged (an orphaned file only wastes space). Returns false when
    /// a remote disk could not be asked (its node did not answer), so the caller may retry later.
    pub async fn remove(&self, dest: Dest) -> bool {
        match self {
            Self::Local(d) => {
                blob::remove_path(&dest.path(d)).await;
                true
            }
            Self::Remote { id, peer, .. } => match peer.remove(*id, dest).await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(disk = %self.label(), ?dest, %error, "remote remove failed");
                    false
                }
            },
        }
    }

    /// Like [`DiskRef::remove`], for the parts directory of an upload.
    pub async fn remove_upload(&self, upload: Uuid) -> bool {
        match self {
            Self::Local(d) => {
                blob::remove_upload_dir(d, upload).await;
                true
            }
            Self::Remote { id, peer, .. } => match peer.remove_upload(*id, upload).await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(disk = %self.label(), %upload, %error, "remote upload cleanup failed");
                    false
                }
            },
        }
    }

    pub async fn concat(
        &self,
        upload: Uuid,
        parts: Vec<blob::JoinPart>,
        object: Uuid,
    ) -> io::Result<String> {
        match self {
            Self::Local(d) => blob::concat_parts(d, upload, &parts, object).await,
            Self::Remote { id, peer, .. } => peer.concat(*id, upload, parts, object).await,
        }
    }
}
