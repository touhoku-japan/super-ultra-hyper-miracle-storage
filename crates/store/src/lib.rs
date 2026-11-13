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
    meta::{AccessKeyRecord, BucketRecord, ObjectRecord, TenantRecord},
    store::{PutObjectRequest, RepairReport, ScrubReport, Store},
};
