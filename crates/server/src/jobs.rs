//! Maintenance jobs started from the operator console (the same work as `suhms scrub`,
//! `suhms repair`, ...), run in the background of this server process. Scrub and gc work on
//! this node's disks; repair and rebalance take the cluster-wide lease, so they fail with "busy"
//! while another node runs one.

use std::{collections::BTreeMap, sync::Arc, sync::Mutex, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::json;
use suhms_store::{GcOptions, RebalanceOptions, Store, StoreError};

use crate::admin::now_millis;

pub const JOBS: &[&str] = &[
    "scrub",
    "repair",
    "repair-full",
    "gc",
    "rebalance",
    "checksum-backfill",
];

/// Options of a job run; each job reads the ones that apply to it.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JobParams {
    /// gc: leave files younger than this (default: `[maintenance] gc_grace_secs`).
    pub grace_secs: Option<u64>,
    /// gc: also on disks for which the metadata records no copies.
    pub force: bool,
    /// rebalance: stop when free space differs by at most this many GiB (default 10).
    pub margin_gib: Option<u64>,
    /// rebalance: stop after this many moves.
    pub max_moves: Option<u64>,
    /// checksum-backfill: stop after this many objects.
    pub max_objects: Option<u64>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct JobState {
    pub name: &'static str,
    pub running: bool,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// Report of the last finished run.
    pub result: Option<serde_json::Value>,
    /// Error of the last finished run.
    pub error: Option<String>,
}

pub struct Jobs {
    store: Arc<Store>,
    gc_grace: Duration,
    backfill_bytes_per_sec: Option<u64>,
    state: Mutex<BTreeMap<&'static str, JobState>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StartError {
    Unknown,
    Running,
}

impl Jobs {
    pub fn new(store: Arc<Store>, gc_grace: Duration, backfill_mib_per_sec: u64) -> Self {
        Self {
            store,
            gc_grace,
            backfill_bytes_per_sec: (backfill_mib_per_sec > 0)
                .then_some(backfill_mib_per_sec << 20),
            state: Mutex::new(
                JOBS.iter()
                    .map(|n| {
                        (
                            *n,
                            JobState {
                                name: n,
                                ..JobState::default()
                            },
                        )
                    })
                    .collect(),
            ),
        }
    }

    pub fn list(&self) -> Vec<JobState> {
        self.state
            .lock()
            .expect("jobs lock")
            .values()
            .cloned()
            .collect()
    }

    /// Starts `name` in the background.
    pub fn start(self: &Arc<Self>, name: &str, params: JobParams) -> Result<(), StartError> {
        let name = *JOBS
            .iter()
            .find(|n| **n == name)
            .ok_or(StartError::Unknown)?;
        {
            let mut state = self.state.lock().expect("jobs lock");
            let s = state.get_mut(name).expect("every job has a state");
            if s.running {
                return Err(StartError::Running);
            }
            s.running = true;
            s.started_at = Some(now_millis());
        }
        tracing::info!(target: "suhms::audit", job = name, ?params, "maintenance job started from the console");
        let jobs = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = jobs.run(name, params).await;
            let mut state = jobs.state.lock().expect("jobs lock");
            let s = state.get_mut(name).expect("every job has a state");
            s.running = false;
            s.finished_at = Some(now_millis());
            match outcome {
                Ok(v) => {
                    tracing::info!(job = name, result = %v, "maintenance job finished");
                    s.result = Some(v);
                    s.error = None;
                }
                Err(e) => {
                    tracing::warn!(job = name, error = %e, "maintenance job failed");
                    s.result = None;
                    s.error = Some(e);
                }
            }
        });
        Ok(())
    }

    async fn run(&self, name: &str, p: JobParams) -> Result<serde_json::Value, String> {
        let s = &self.store;
        let err = |e: StoreError| match e {
            StoreError::Busy(reason) => format!("別の処理が実行中です（{reason}）"),
            e => e.to_string(),
        };
        Ok(match name {
            "scrub" => {
                let r = s.scrub().await.map_err(err)?;
                json!({ "checked": r.checked, "removed": r.removed })
            }
            "repair" | "repair-full" => {
                let r = if name == "repair" {
                    s.repair().await
                } else {
                    s.repair_full().await
                }
                .map_err(err)?;
                json!({
                    "copied": r.copied, "trimmed": r.trimmed, "drained": r.drained,
                    "queued": r.queued, "postponed": r.postponed, "resumed": r.resumed,
                    "unrecoverable": r.unrecoverable,
                })
            }
            "gc" => {
                let r = s
                    .gc(GcOptions {
                        grace: p.grace_secs.map_or(self.gc_grace, Duration::from_secs),
                        force: p.force,
                    })
                    .await
                    .map_err(err)?;
                json!({
                    "orphans": r.orphans, "orphan_bytes": r.orphan_bytes, "uploads": r.uploads,
                    "tmp": r.tmp, "purged": r.purged, "skipped_disks": r.skipped_disks,
                })
            }
            "rebalance" => {
                let r = s
                    .rebalance(RebalanceOptions {
                        margin: p.margin_gib.unwrap_or(10) << 30,
                        max_moves: p.max_moves,
                    })
                    .await
                    .map_err(err)?;
                json!({ "moved": r.moved, "bytes": r.bytes })
            }
            "checksum-backfill" => {
                let r = crate::backfill::backfill_checksums(
                    s,
                    crate::backfill::BackfillOptions {
                        max_objects: p.max_objects,
                        bytes_per_sec: self.backfill_bytes_per_sec,
                    },
                )
                .await
                .map_err(err)?;
                json!({
                    "updated": r.updated, "corrupt": r.corrupt, "failed": r.failed,
                    "remaining": r.remaining,
                })
            }
            _ => return Err("unknown job".into()),
        })
    }
}
