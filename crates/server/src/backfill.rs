//! Adds checksums to objects stored before every upload got one.
//!
//! Such objects get the checksum new uploads get by default: a full-object CRC64NVME, computed
//! from a copy whose content matches the object's BLAKE3 (so a corrupt copy never yields a
//! checksum). It is safe while the server runs; a cluster-wide lease keeps two runs apart.

use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use s3s::crypto::{Checksum as _, Crc64Nvme};
use suhms_store::Store;

use crate::checksum::{Algo, FULL_OBJECT, Stored};

const LEASE: &str = "checksums";
const PAGE: u32 = 500;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackfillOptions {
    /// Stop after this many objects (the rest is left for the next run).
    pub max_objects: Option<u64>,
    /// Read at most this many bytes per second, to spare the disks; `None` = unlimited.
    pub bytes_per_sec: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackfillReport {
    /// Objects that got a checksum.
    pub updated: u64,
    /// Objects whose copy did not match its BLAKE3 (left for scrub and repair to fix).
    pub corrupt: u64,
    /// Objects that could not be read.
    pub failed: u64,
    /// Whether the run stopped at `max_objects` with objects left.
    pub remaining: bool,
}

impl BackfillReport {
    /// Nothing was left to do.
    pub fn is_idle(&self) -> bool {
        *self == Self::default()
    }
}

/// The checksum a whole object's bytes get (as stored in the `checksum` column).
fn stored_crc64nvme(crc: Crc64Nvme) -> String {
    Stored {
        algorithm: Algo::Crc64Nvme.name().to_owned(),
        kind: FULL_OBJECT.to_owned(),
        value: B64.encode(crc.finalize()),
    }
    .encode()
}

pub async fn backfill_checksums(
    store: &Store,
    options: BackfillOptions,
) -> suhms_store::Result<BackfillReport> {
    let lease = store.lease(LEASE).await?;
    let meta = store.metadata();
    let mut report = BackfillReport::default();
    let started = Instant::now();
    let mut bytes = 0u64;
    let mut after = None;
    'pages: loop {
        let ids = meta.list_objects_without_checksum(after, PAGE).await?;
        let Some(last) = ids.last() else { break };
        after = Some(*last);
        for id in ids {
            lease.check()?;
            if options
                .max_objects
                .is_some_and(|m| report.updated + report.corrupt + report.failed >= m)
            {
                report.remaining = true;
                break 'pages;
            }
            let Some(stored) = meta.get_object_by_id(id).await? else {
                continue;
            };
            if stored.object.delete_marker || stored.object.checksum.is_some() {
                continue;
            }
            let mut crc = Crc64Nvme::new();
            let read = store
                .read_verified(&stored, &mut |chunk| crc.update(chunk))
                .await;
            match read {
                Ok(true) => {
                    if meta.set_object_checksum(id, &stored_crc64nvme(crc)).await? {
                        report.updated += 1;
                    }
                }
                Ok(false) => {
                    tracing::warn!(object = %id, bucket = %stored.object.bucket, key = %stored.object.key,
                        "copy does not match its hash; no checksum added (scrub will replace it)");
                    report.corrupt += 1;
                }
                Err(error) => {
                    tracing::warn!(object = %id, bucket = %stored.object.bucket, key = %stored.object.key,
                        %error, "cannot read object for checksum backfill");
                    report.failed += 1;
                }
            }
            bytes += stored.object.size;
            if let Some(rate) = options.bytes_per_sec.filter(|r| *r > 0) {
                let due = Duration::from_secs_f64(bytes as f64 / rate as f64);
                if let Some(wait) = due.checked_sub(started.elapsed()) {
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
    lease.release().await?;
    Ok(report)
}
