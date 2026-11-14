//! Storage engine: stores each object as plain files, one full copy per disk, spread across
//! nodes, with the bucket/object index in a pluggable metadata database.

pub mod blob;
pub mod cluster;
pub mod config;
pub mod disk;
pub mod error;
pub mod lifecycle;
pub mod list;
pub mod lock;
pub mod meta;
pub mod placement;
mod store;

pub use crate::{
    cluster::{BoxReader, DiskRef, NoPeers, Peer, Peers, ReplicaSink},
    config::StoreConfig,
    error::{Result, StoreError},
    list::{ListPage, ListParams, ListStart},
    meta::{
        AccessKeyRecord, BucketRecord, DiskRecord, NULL_VERSION, ObjectRecord, PartRecord,
        RemoveDiskOutcome, RepairQueueEntry, StorageUsage, TenantRecord, TenantUpdate,
        UploadRecord, UsageDelta, UsageRow, VersioningMode, WriteCondition,
    },
    store::{
        ChecksumSlot, CompleteChecksum, CompletedPart, CopySource, CopyWrap, DeleteResult,
        DiskInfo, GcOptions, GcReport, HEARTBEAT_INTERVAL, Lease, LifecycleReport, MAX_PART_NUMBER,
        MIN_PART_SIZE, PartsPage, PutObjectRequest, RESERVED_BUCKETS, RebalanceOptions,
        RebalanceReport, RepairReport, ScrubReport, Store, UploadsPage, VersionsPage, hour_of,
        validate_tags,
    },
};
