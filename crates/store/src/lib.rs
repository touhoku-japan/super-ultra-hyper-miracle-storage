//! Storage engine: stores each object as plain files, one full copy per disk, spread across
//! nodes, with the bucket/object index in a pluggable metadata database.

mod blob;
pub mod config;
pub mod disk;
pub mod error;
pub mod list;
pub mod meta;
pub mod placement;
mod store;

pub use crate::{
    config::StoreConfig,
    error::{Result, StoreError},
    list::{ListPage, ListParams, ListStart},
    meta::{AccessKeyRecord, BucketRecord, ObjectRecord, PartRecord, TenantRecord, UploadRecord},
    store::{
        CompletedPart, MAX_PART_NUMBER, MIN_PART_SIZE, PartsPage, PutObjectRequest, RepairReport,
        ScrubReport, Store, UploadsPage,
    },
};
