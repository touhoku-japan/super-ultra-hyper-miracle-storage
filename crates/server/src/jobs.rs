//! Maintenance jobs (the same work as `suhms scrub`, `suhms repair`, ...), run in the background
//! of this server process: on the `[maintenance]` schedule, or when started from the operator
//! console. Scrub and gc work on this node's disks; repair, rebalance and the checksum backfill
//! take a cluster-wide lease, so they fail with "busy" while another node runs one.
//!
//! Every run is recorded in the metadata DB (`job_runs`, one row per node and job), so the
//! console of any node shows what each node is doing and how its last run ended. A running row
//! carries a heartbeat; a row whose node stopped updating it (crashed) counts as interrupted.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use suhms_store::{GcOptions, RebalanceOptions, Store, StoreError, meta::JobRun};

use crate::{admin::now_millis, config::Config};

/// How often a running job refreshes its heartbeat.
const HEARTBEAT: Duration = Duration::from_secs(30);
/// A running row older than this belongs to a node that stopped (crashed or hung).
const STALE_MS: i64 = 120_000;

/// Where a job works: this node's disks, or the whole cluster (one node at a time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Node,
    Cluster,
}

/// In the order the console lists them (most used first).
pub const JOBS: &[(&str, Scope)] = &[
    ("repair", Scope::Cluster),
    ("repair-full", Scope::Cluster),
    ("scrub", Scope::Node),
    ("gc", Scope::Node),
    ("rebalance", Scope::Cluster),
    ("checksum-backfill", Scope::Cluster),
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

/// A job as the console shows it: its last run on every node.
#[derive(Debug, Clone, Serialize)]
pub struct JobInfo {
    pub name: &'static str,
    pub scope: Scope,
    /// Starting it here now would be refused (it runs on this node, or for a cluster-wide job,
    /// on any node).
    pub blocked: bool,
    pub runs: Vec<RunInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunInfo {
    pub node_id: String,
    /// The node whose API answered.
    pub this_node: bool,
    pub running: bool,
    /// It was running when its node stopped answering (no heartbeat for a while).
    pub interrupted: bool,
    /// `"console"` or `"schedule"`.
    pub started_by: String,
    pub started_at: i64,
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
    /// Jobs running in this process (whatever started them).
    local: Mutex<HashSet<&'static str>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StartError {
    Unknown,
    /// Runs already; on which node.
    Running(String),
    Metadata(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartedBy {
    Console,
    Schedule,
}

impl StartedBy {
    fn as_str(self) -> &'static str {
        match self {
            Self::Console => "console",
            Self::Schedule => "schedule",
        }
    }
}

/// Removes the job from the running set when the run ends (also on panic).
struct LocalGuard<'a>(&'a Jobs, &'static str);

impl Drop for LocalGuard<'_> {
    fn drop(&mut self) {
        self.0.local.lock().expect("jobs lock").remove(self.1);
    }
}

fn job(name: &str) -> Option<(&'static str, Scope)> {
    JOBS.iter().copied().find(|(n, _)| *n == name)
}

/// True when every count in a job's report is zero (the run found nothing to do).
pub fn is_idle(result: &serde_json::Value) -> bool {
    result.as_object().is_some_and(|m| {
        m.values()
            .all(|v| v.as_u64() == Some(0) || v.as_bool() == Some(false))
    })
}

impl Jobs {
    pub fn new(store: Arc<Store>, gc_grace: Duration, backfill_mib_per_sec: u64) -> Self {
        Self {
            store,
            gc_grace,
            backfill_bytes_per_sec: (backfill_mib_per_sec > 0)
                .then_some(backfill_mib_per_sec << 20),
            local: Mutex::new(HashSet::new()),
        }
    }

    pub fn from_config(store: Arc<Store>, config: &Config) -> Self {
        let m = &config.maintenance;
        Self::new(
            store,
            Duration::from_secs(m.gc_grace_secs),
            m.checksum_backfill_mib_per_sec,
        )
    }

    fn node_id(&self) -> &str {
        self.store.node_id()
    }

    /// Marks this node's runs that were still "running" as interrupted (they died with the
    /// previous process). Call once at startup, before any job runs.
    pub async fn recover(&self) -> suhms_store::Result<()> {
        let now = now_millis();
        for mut run in self.store.metadata().list_job_runs().await? {
            if run.node_id == self.node_id() && run.running {
                run.running = false;
                run.finished_at = Some(now);
                run.result = None;
                run.error = Some("サーバの再起動で中断されました".into());
                self.store.metadata().put_job_run(&run).await?;
            }
        }
        Ok(())
    }

    /// Every job with its last run on each node.
    pub async fn list(&self) -> suhms_store::Result<Vec<JobInfo>> {
        let now = now_millis();
        let runs = self.store.metadata().list_job_runs().await?;
        let local = self.local.lock().expect("jobs lock").clone();
        Ok(JOBS
            .iter()
            .map(|&(name, scope)| {
                let runs: Vec<RunInfo> = runs
                    .iter()
                    .filter(|r| r.job == name)
                    .map(|r| self.run_info(r, now))
                    .collect();
                let blocked = local.contains(name)
                    || (scope == Scope::Cluster && runs.iter().any(|r| r.running));
                JobInfo {
                    name,
                    scope,
                    blocked,
                    runs,
                }
            })
            .collect())
    }

    fn run_info(&self, r: &JobRun, now: i64) -> RunInfo {
        let stale = r.running && r.heartbeat_at < now - STALE_MS;
        RunInfo {
            node_id: r.node_id.clone(),
            this_node: r.node_id == self.node_id(),
            running: r.running && !stale,
            interrupted: stale,
            started_by: r.started_by.clone(),
            started_at: r.started_at,
            finished_at: r.finished_at,
            result: r
                .result
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok()),
            error: if stale {
                Some("中断されました（ノードからの応答がありません）".into())
            } else {
                r.error.clone()
            },
        }
    }

    /// Starts `name` in the background (from the console).
    pub async fn start(self: &Arc<Self>, name: &str, params: JobParams) -> Result<(), StartError> {
        let (name, scope) = job(name).ok_or(StartError::Unknown)?;
        if scope == Scope::Cluster {
            let now = now_millis();
            let runs = self
                .store
                .metadata()
                .list_job_runs()
                .await
                .map_err(|e| StartError::Metadata(e.to_string()))?;
            if let Some(r) = runs
                .iter()
                .find(|r| r.job == name && r.node_id != self.node_id() && r.running)
                .filter(|r| r.heartbeat_at >= now - STALE_MS)
            {
                return Err(StartError::Running(r.node_id.clone()));
            }
        }
        if !self.local.lock().expect("jobs lock").insert(name) {
            return Err(StartError::Running(self.node_id().to_owned()));
        }
        tracing::info!(target: "suhms::audit", job = name, ?params, "maintenance job started from the console");
        let jobs = Arc::clone(self);
        tokio::spawn(async move {
            let _guard = LocalGuard(&jobs, name);
            let _ = jobs.run_recorded(name, params, StartedBy::Console).await;
        });
        Ok(())
    }

    /// Runs `name` now on the `[maintenance]` schedule. Skipped (`Ok(None)`) while the console
    /// runs the same job in this process, or when another node holds the job's lease.
    pub async fn run_scheduled(&self, name: &str) -> Result<Option<serde_json::Value>, StoreError> {
        let Some((name, _)) = job(name) else {
            return Ok(None);
        };
        if !self.local.lock().expect("jobs lock").insert(name) {
            return Ok(None);
        }
        let _guard = LocalGuard(self, name);
        match self
            .run_recorded(name, JobParams::default(), StartedBy::Schedule)
            .await
        {
            Ok(v) => Ok(Some(v)),
            Err(StoreError::Busy(reason)) => {
                tracing::debug!(job = name, %reason, "skipped");
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Runs a job while keeping its row in `job_runs` up to date. A scheduled run that finds the
    /// lease taken puts the previous row back (it did nothing worth showing).
    async fn run_recorded(
        &self,
        name: &'static str,
        params: JobParams,
        by: StartedBy,
    ) -> Result<serde_json::Value, StoreError> {
        let meta = self.store.metadata();
        let previous = match by {
            StartedBy::Schedule => {
                meta.get_job_run(self.node_id(), name)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(job = name, error = %e, "cannot read job state");
                        None
                    })
            }
            StartedBy::Console => None,
        };
        let now = now_millis();
        let mut row = JobRun {
            node_id: self.node_id().to_owned(),
            job: name.to_owned(),
            running: true,
            started_by: by.as_str().to_owned(),
            started_at: now,
            finished_at: None,
            heartbeat_at: now,
            result: None,
            error: None,
        };
        self.save(&row).await;

        // The heartbeat stops on a signal (not abort) so a write in flight finishes before the
        // final state is written.
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let beat = {
            let store = Arc::clone(&self.store);
            let mut row = row.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(HEARTBEAT);
                tick.tick().await;
                loop {
                    tokio::select! {
                        _ = &mut stopped => break,
                        _ = tick.tick() => {}
                    }
                    row.heartbeat_at = now_millis();
                    if let Err(e) = store.metadata().put_job_run(&row).await {
                        tracing::warn!(job = row.job, error = %e, "cannot update job heartbeat");
                    }
                }
            })
        };
        let outcome = self.run(name, params).await;
        let _ = stop.send(());
        let _ = beat.await;

        if by == StartedBy::Schedule
            && let Err(StoreError::Busy(_)) = &outcome
        {
            let restored = match &previous {
                Some(p) => meta.put_job_run(p).await,
                None => meta.delete_job_run(self.node_id(), name).await,
            };
            if let Err(e) = restored {
                tracing::warn!(job = name, error = %e, "cannot record job state");
            }
            return outcome;
        }

        let end = now_millis();
        row.running = false;
        row.finished_at = Some(end);
        row.heartbeat_at = end;
        match &outcome {
            Ok(v) => {
                if by == StartedBy::Console || !is_idle(v) {
                    tracing::info!(job = name, started_by = by.as_str(), result = %v, "maintenance job finished");
                }
                row.result = Some(v.to_string());
            }
            Err(e) => {
                tracing::warn!(job = name, started_by = by.as_str(), error = %e, "maintenance job failed");
                row.error = Some(describe(e));
            }
        }
        self.save(&row).await;
        outcome
    }

    async fn save(&self, row: &JobRun) {
        if let Err(e) = self.store.metadata().put_job_run(row).await {
            tracing::warn!(job = row.job, error = %e, "cannot record job state");
        }
    }

    async fn run(&self, name: &str, p: JobParams) -> Result<serde_json::Value, StoreError> {
        let s = &self.store;
        Ok(match name {
            "scrub" => {
                let r = s.scrub().await?;
                json!({ "checked": r.checked, "removed": r.removed })
            }
            "repair" | "repair-full" => {
                let r = if name == "repair" {
                    s.repair().await
                } else {
                    s.repair_full().await
                }?;
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
                    .await?;
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
                    .await?;
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
                .await?;
                json!({
                    "updated": r.updated, "corrupt": r.corrupt, "failed": r.failed,
                    "remaining": r.remaining,
                })
            }
            _ => unreachable!("job() only returns known jobs"),
        })
    }
}

/// The message the console shows for a failed run.
fn describe(e: &StoreError) -> String {
    match e {
        StoreError::Busy(reason) => format!("別の処理が実行中です（{reason}）"),
        e => e.to_string(),
    }
}
