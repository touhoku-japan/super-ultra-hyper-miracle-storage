//! Background maintenance: scrub, repair, garbage collection of orphaned files, draining
//! (decommissioning) disks, and rebalancing copies across disks.
//!
//! Repair and rebalance change where copies live, so they hold the cluster-wide `replicas`
//! lease (see [`Lease`]): any node may run them, but only one at a time.

use std::{
    collections::{HashMap, HashSet},
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{fs, task::JoinHandle};
use uuid::Uuid;

use super::{ALIVE_MS, PAGE, RepairReport, ScrubReport, Store, now_millis};
use crate::{
    blob::{self, CopyOutcome, Dest},
    cluster::DiskRef,
    disk::{Disk, DiskId},
    error::{Result, StoreError},
    meta::{MetadataStore, RemoveDiskOutcome, StoredObject},
};

/// Lease shared by every job that adds, moves or removes copies.
const REPLICAS_LEASE: &str = "replicas";
const LEASE_TTL: Duration = Duration::from_secs(60);
const LEASE_RENEW: Duration = Duration::from_secs(20);

/// Holds a cluster-wide lease while a job runs. A background task renews it; if renewal fails
/// for longer than the TTL (or another holder took over), [`Lease::check`] fails so the job
/// stops. Dropping it gives the lease up.
pub struct Lease {
    meta: Arc<dyn MetadataStore>,
    name: &'static str,
    holder: String,
    lost: Arc<AtomicBool>,
    renew: JoinHandle<()>,
    released: bool,
}

impl Lease {
    /// Gives the lease up now. (Dropping it does so in the background, which a process that
    /// is about to exit may never get to.)
    pub async fn release(mut self) -> Result<()> {
        self.renew.abort();
        self.released = true;
        self.meta.release_lease(self.name, &self.holder).await
    }

    /// Fails once the lease is lost; long jobs call it between steps and stop on error.
    pub fn check(&self) -> Result<()> {
        if self.lost.load(Ordering::Relaxed) {
            return Err(StoreError::Busy(format!(
                "lost the {} lease; another node took over",
                self.name
            )));
        }
        Ok(())
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.renew.abort();
        if self.released {
            return;
        }
        let (meta, name, holder) = (Arc::clone(&self.meta), self.name, self.holder.clone());
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                if let Err(error) = meta.release_lease(name, &holder).await {
                    tracing::warn!(lease = name, %error, "failed to release lease; it expires on its own");
                }
            });
        }
    }
}

/// Wait before retrying an object that could not be repaired: 10 minutes, doubling with
/// every failed attempt, at most 6 hours.
fn retry_delay_ms(attempts: u32) -> i64 {
    const BASE: i64 = 10 * 60 * 1000;
    const MAX: i64 = 6 * 3600 * 1000;
    (BASE << attempts.min(6)).min(MAX)
}

fn ttl_ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// What [`Store::gc`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcOptions {
    /// Files younger than this are never touched (they may belong to a write that has not
    /// committed its metadata yet). Also how long collected files stay in the trash.
    pub grace: Duration,
    /// Collect even on a disk for which the metadata has no copies at all. Without it such a
    /// disk is skipped, since that usually means the wrong metadata database.
    pub force: bool,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(24 * 3600),
            force: false,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GcReport {
    /// Object files without a replica record, moved to the disk's trash (`.suhms/gc/`).
    pub orphans: u64,
    pub orphan_bytes: u64,
    /// Leftover directories of multipart uploads that no longer exist.
    pub uploads: u64,
    /// Stale temp files of interrupted writes.
    pub tmp: u64,
    /// Trash directories older than the grace period, deleted for good.
    pub purged: u64,
    /// Disks skipped because the metadata has no copies on them (see [`GcOptions::force`]).
    pub skipped_disks: u64,
}

/// What [`Store::rebalance`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebalanceOptions {
    /// Stop once the most and least free usable disks differ by at most this many bytes.
    pub margin: u64,
    /// Stop after this many moved copies.
    pub max_moves: Option<u64>,
}

impl Default for RebalanceOptions {
    fn default() -> Self {
        Self {
            margin: 10 << 30,
            max_moves: None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RebalanceReport {
    pub moved: u64,
    pub bytes: u64,
}

const TRASH_DIR: &str = "gc";

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

async fn modified(path: &Path) -> io::Result<SystemTime> {
    fs::metadata(path).await?.modified()
}

/// Entries of a directory (a missing directory is empty).
async fn entries(dir: &Path) -> io::Result<Vec<fs::DirEntry>> {
    let mut rd = match fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    while let Some(e) = rd.next_entry().await? {
        out.push(e);
    }
    out.sort_by_key(fs::DirEntry::file_name);
    Ok(out)
}

fn is_hex(name: &str, len: usize) -> bool {
    name.len() == len
        && name
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

impl Store {
    /// Takes a cluster-wide lease for a job, renewed in the background until released or
    /// dropped. Fails with [`StoreError::Busy`] while another process holds it.
    pub async fn lease(&self, name: &'static str) -> Result<Lease> {
        let ttl = ttl_ms(LEASE_TTL);
        if !self
            .meta
            .try_acquire_lease(name, &self.instance, now_millis(), ttl)
            .await?
        {
            return Err(StoreError::Busy(format!(
                "another node is running a job that holds the {name} lease (repair or rebalance)"
            )));
        }
        let lost = Arc::new(AtomicBool::new(false));
        let renew = {
            let (meta, holder, lost) = (
                Arc::clone(&self.meta),
                self.instance.clone(),
                Arc::clone(&lost),
            );
            tokio::spawn(async move {
                let mut last_ok = now_millis();
                loop {
                    tokio::time::sleep(LEASE_RENEW).await;
                    match meta
                        .try_acquire_lease(name, &holder, now_millis(), ttl)
                        .await
                    {
                        Ok(true) => last_ok = now_millis(),
                        Ok(false) => {
                            lost.store(true, Ordering::Relaxed);
                            return;
                        }
                        Err(error) => {
                            tracing::warn!(lease = name, %error, "lease renewal failed");
                            if now_millis() - last_ok > ttl {
                                lost.store(true, Ordering::Relaxed);
                                return;
                            }
                        }
                    }
                }
            })
        };
        Ok(Lease {
            meta: Arc::clone(&self.meta),
            name,
            holder: self.instance.clone(),
            lost,
            renew,
            released: false,
        })
    }

    // ---- scrub ----

    /// Re-hashes every copy on this node's disks and drops copies that are missing or corrupt,
    /// so that [`Store::repair`] replaces them.
    pub async fn scrub(&self) -> Result<ScrubReport> {
        let mut report = ScrubReport::default();
        let mut after = None;
        loop {
            let page = self.meta.list_replicas(after, PAGE).await?;
            let Some(last) = page.last() else { break };
            after = Some((last.object_id, last.disk_id));
            for entry in &page {
                let Some(disk) = self.disk(entry.disk_id) else {
                    continue;
                };
                report.checked += 1;
                let path = disk.object_path(entry.object_id);
                let bad = match blob::hash_file(&path).await {
                    Ok((_, hash)) => hash != entry.blake3,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => true,
                    Err(error) => {
                        // Possibly a failing disk rather than a bad file; leave it for now.
                        tracing::warn!(path = %path.display(), %error, "cannot read replica during scrub");
                        false
                    }
                };
                if bad {
                    tracing::warn!(path = %path.display(), "dropping missing or corrupt replica");
                    // Also queues the object for repair.
                    self.meta
                        .remove_replica(entry.object_id, entry.disk_id)
                        .await?;
                    blob::remove(disk, entry.object_id).await;
                    report.removed += 1;
                }
            }
        }
        Ok(report)
    }

    // ---- repair ----

    /// Brings queued objects to their desired number of copies: copies from verified healthy
    /// copies, removes surplus ones, and moves copies off draining disks.
    ///
    /// Objects are queued when they are written short of copies, lose a copy, or have their
    /// desired count changed; this pass also queues every object of a disk that went down,
    /// came back (to trim the copies made meanwhile) or started draining. Nothing else is
    /// scanned; [`Store::repair_full`] additionally checks every object.
    ///
    /// Fails with [`StoreError::Busy`] while another repair or rebalance runs anywhere.
    pub async fn repair(&self) -> Result<RepairReport> {
        self.run_repair(false).await
    }

    /// [`Store::repair`] plus a scan of every object, as a safety net for anything the queue
    /// missed. Expensive on large clusters; run it rarely.
    pub async fn repair_full(&self) -> Result<RepairReport> {
        self.run_repair(true).await
    }

    async fn run_repair(&self, full: bool) -> Result<RepairReport> {
        let lease = self.lease(REPLICAS_LEASE).await?;
        self.refresh_cluster().await;
        let mut report = RepairReport::default();
        self.queue_disk_changes(&mut report).await?;

        let mut after = None;
        let now = now_millis();
        loop {
            let entries = self.meta.list_repair_queue(after, PAGE, now).await?;
            let Some(last) = entries.last() else { break };
            after = Some(last.object_id);
            for entry in entries {
                lease.check()?;
                let id = entry.object_id;
                let done = match self.meta.get_object_by_id(id).await? {
                    Some(stored) => self.repair_object(&stored, &mut report).await?,
                    None => true,
                };
                if done {
                    self.meta.dequeue_repair(id).await?;
                } else {
                    // Probably nowhere to put a copy right now (e.g. a node is down): wait
                    // instead of retrying every pass. Queuing it again, or new capacity, ends
                    // the wait early.
                    self.meta
                        .postpone_repair(id, now_millis() + retry_delay_ms(entry.attempts))
                        .await?;
                    report.postponed += 1;
                }
            }
        }

        if full {
            let mut after = None;
            loop {
                let ids = self
                    .meta
                    .list_misreplicated(after, PAGE, now_millis() - ALIVE_MS)
                    .await?;
                let Some(last) = ids.last() else { break };
                after = Some(*last);
                for id in ids {
                    lease.check()?;
                    if let Some(stored) = self.meta.get_object_by_id(id).await?
                        && !self.repair_object(&stored, &mut report).await?
                    {
                        // Not fixable now: keep it in front of the regular passes.
                        self.meta.enqueue_repair(id).await?;
                    }
                }
            }
        }
        lease.release().await?;
        Ok(report)
    }

    /// Queues the objects of disks that went down or came back since the last pass, and
    /// ends the backoff of postponed objects when usable disks were added (or came back).
    async fn queue_disk_changes(&self, report: &mut RepairReport) -> Result<()> {
        let view = self.cluster_disks().await?;
        let usable: HashSet<DiskId> = view
            .iter()
            .filter(|i| i.alive && !i.draining)
            .map(|i| i.id)
            .collect();
        let grew = {
            let mut last = self.repair_capacity.lock().expect("lock poisoned");
            let grew = last.as_ref().is_none_or(|l| !usable.is_subset(l));
            *last = Some(usable);
            grew
        };
        if grew {
            report.resumed += self.meta.reset_repair_backoff().await?;
        }
        for record in self.meta.list_disks().await? {
            let alive = view.iter().any(|i| i.id == record.id && i.alive);
            match (alive, record.down_since) {
                (false, None) => {
                    let n = self.meta.enqueue_disk_repairs(record.id).await?;
                    self.meta
                        .set_disk_down_since(record.id, Some(now_millis()))
                        .await?;
                    tracing::warn!(disk = %record.id, node = %record.node_id, queued = n,
                        "disk is down; queued its objects for repair");
                    report.queued += n;
                }
                (true, Some(_)) => {
                    let n = self.meta.enqueue_disk_repairs(record.id).await?;
                    self.meta.set_disk_down_since(record.id, None).await?;
                    tracing::info!(disk = %record.id, node = %record.node_id, queued = n,
                        "disk is back; queued its objects to trim surplus copies");
                    report.queued += n;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Brings one object to its desired number of copies on live, non-draining disks, then
    /// drops its copies on draining disks. Returns whether it is now fully replicated (so it
    /// can leave the queue).
    async fn repair_object(
        &self,
        stored: &StoredObject,
        report: &mut RepairReport,
    ) -> Result<bool> {
        let id = stored.object.id;
        let desired = usize::from(stored.object.desired_replicas);
        let view = self.cluster_disks().await?;
        let draining_disk = |d: &DiskId| view.iter().any(|i| i.id == *d && i.draining);

        let mut healthy = Vec::new();
        let mut draining = Vec::new();
        for info in stored
            .replicas
            .iter()
            .filter_map(|d| view.iter().find(|i| i.id == *d))
        {
            if let Some(r) = self.reach(info) {
                if info.draining {
                    draining.push(r);
                } else {
                    healthy.push(r);
                }
            }
        }
        healthy.sort_by_key(|d| !d.is_local());
        draining.sort_by_key(|d| !d.is_local());

        if healthy.len() > desired {
            // Remove surplus copies from the fullest disks first.
            let free = |d: &DiskRef| view.iter().find(|i| i.id == d.id()).map_or(0, |i| i.free);
            healthy.sort_by_key(free);
            let surplus = healthy.len() - desired;
            for disk in healthy.drain(..surplus) {
                self.meta.remove_replica(id, disk.id()).await?;
                disk.remove(Dest::Object(id)).await;
                report.trimmed += 1;
            }
        } else if healthy.len() < desired {
            // Copies on draining disks do not count, so their node may take the new copy.
            let keep: Vec<DiskId> = stored
                .replicas
                .iter()
                .filter(|d| !draining_disk(d))
                .copied()
                .collect();
            let targets = self
                .place(desired - healthy.len(), &keep, &stored.replicas)
                .await?;
            let mut sources: Vec<DiskRef> = healthy.iter().chain(&draining).cloned().collect();
            for target in targets {
                loop {
                    let Some(source) = sources.first().cloned() else {
                        tracing::error!(object = %id, bucket = %stored.object.bucket, key = %stored.object.key,
                            "no healthy copy on a reachable disk; cannot repair");
                        report.unrecoverable += 1;
                        return Ok(false);
                    };
                    match blob::copy_replica(
                        &source,
                        &target,
                        id,
                        stored.object.size,
                        &stored.object.blake3,
                    )
                    .await
                    {
                        Ok(CopyOutcome::Copied) => {
                            if !self.meta.add_replica(id, target.id()).await? {
                                // Object was deleted meanwhile.
                                target.remove(Dest::Object(id)).await;
                                return Ok(true);
                            }
                            report.copied += 1;
                            healthy.push(target.clone());
                            break;
                        }
                        Ok(CopyOutcome::SourceCorrupt) => {
                            tracing::warn!(object = %id, disk = %source.label(), "source copy is corrupt; dropping it");
                            self.meta.remove_replica(id, source.id()).await?;
                            source.remove(Dest::Object(id)).await;
                            sources.remove(0);
                            healthy.retain(|d| d.id() != source.id());
                            draining.retain(|d| d.id() != source.id());
                        }
                        Ok(CopyOutcome::SourceUnreadable) => {
                            // Possibly transient (e.g. a failing disk); try another copy but keep
                            // the record. Scrub decides whether the copy is really gone.
                            sources.remove(0);
                        }
                        Err(error) => {
                            tracing::warn!(object = %id, target = %target.label(), %error, "copy failed");
                            break;
                        }
                    }
                }
            }
        }

        if healthy.len() < desired {
            return Ok(false);
        }
        for disk in draining {
            self.meta.remove_replica(id, disk.id()).await?;
            disk.remove(Dest::Object(id)).await;
            report.drained += 1;
        }
        // Records on disks that are down stay: if the disk comes back, repair is queued again
        // and trims the surplus; a dead disk's records go when the disk is removed.
        Ok(true)
    }

    // ---- draining and removing disks ----

    /// Starts (or stops) draining a disk: it gets no new copies, and repair copies its objects
    /// elsewhere and then deletes them from it. Returns false when the disk does not exist.
    pub async fn set_disk_draining(&self, disk: DiskId, draining: bool) -> Result<bool> {
        if !self.meta.set_disk_draining(disk, draining).await? {
            return Ok(false);
        }
        // Draining: every object must move. Undraining: objects already copied elsewhere now
        // have a surplus copy to trim.
        self.meta.enqueue_disk_repairs(disk).await?;
        self.refresh_cluster().await;
        Ok(true)
    }

    /// Forgets a disk (see [`MetadataStore::remove_disk`]).
    pub async fn remove_disk(&self, disk: DiskId, force: bool) -> Result<RemoveDiskOutcome> {
        let outcome = self.meta.remove_disk(disk, force).await?;
        self.refresh_cluster().await;
        Ok(outcome)
    }

    // ---- garbage collection ----

    /// Collects files on this node's disks that no metadata refers to: object files without a
    /// replica record (left by a crash between writing and committing, or by deletes while
    /// the disk was offline), directories of multipart uploads that no longer exist, and stale
    /// temp files. Object files go to the disk's trash first and are deleted for good by a
    /// later run, once they are older than the grace period. Safe while the server runs.
    pub async fn gc(&self, options: GcOptions) -> Result<GcReport> {
        let mut report = GcReport::default();
        let now = SystemTime::now();
        let cutoff = now.checked_sub(options.grace).unwrap_or(UNIX_EPOCH);
        for disk in &self.disks {
            if let Err(error) = self.gc_disk(disk, options, now, cutoff, &mut report).await {
                tracing::warn!(disk = %disk.root.display(), %error, "garbage collection failed");
            }
        }
        Ok(report)
    }

    async fn gc_disk(
        &self,
        disk: &Disk,
        options: GcOptions,
        now: SystemTime,
        cutoff: SystemTime,
        report: &mut GcReport,
    ) -> Result<()> {
        let trash_root = disk.meta_dir().join(TRASH_DIR);
        for dir in entries(&trash_root).await? {
            let name = dir.file_name().to_string_lossy().into_owned();
            let Ok(secs) = name.parse::<u64>() else {
                continue;
            };
            if secs + options.grace.as_secs() < unix_secs(now) {
                fs::remove_dir_all(dir.path()).await?;
                report.purged += 1;
            }
        }

        let objects = disk.objects_dir();
        if !options.force
            && self
                .meta
                .list_disk_replicas(disk.id, None, 1)
                .await?
                .is_empty()
            && has_files(&objects).await?
        {
            tracing::warn!(disk = %disk.root.display(),
                "metadata has no copies on this disk but it holds files; skipping garbage collection (wrong metadata database?)");
            report.skipped_disks += 1;
            return Ok(());
        }
        let trash = trash_root.join(unix_secs(now).to_string());
        for l1 in entries(&objects).await? {
            let a = l1.file_name().to_string_lossy().into_owned();
            if !is_hex(&a, 2) {
                continue;
            }
            for l2 in entries(&l1.path()).await? {
                let b = l2.file_name().to_string_lossy().into_owned();
                if !is_hex(&b, 2) {
                    continue;
                }
                let mut old = Vec::new();
                for f in entries(&l2.path()).await? {
                    let name = f.file_name().to_string_lossy().into_owned();
                    let Ok(id) = Uuid::try_parse(&name) else {
                        continue;
                    };
                    let meta = f.metadata().await?;
                    if meta.is_file() && meta.modified()? < cutoff {
                        old.push((id, f.path(), meta.len()));
                    }
                }
                if old.is_empty() {
                    continue;
                }
                let known: HashSet<Uuid> = self
                    .meta
                    .disk_replicas_with_prefix(disk.id, &format!("{a}{b}"))
                    .await?
                    .into_iter()
                    .collect();
                for (id, path, len) in old {
                    if known.contains(&id) {
                        continue;
                    }
                    fs::create_dir_all(&trash).await?;
                    fs::rename(&path, trash.join(id.simple().to_string())).await?;
                    tracing::info!(disk = %disk.root.display(), object = %id, "moved orphaned file to trash");
                    report.orphans += 1;
                    report.orphan_bytes += len;
                }
            }
        }

        for dir in entries(&disk.multipart_dir()).await? {
            let name = dir.file_name().to_string_lossy().into_owned();
            let Ok(upload) = Uuid::try_parse(&name) else {
                continue;
            };
            if modified(&dir.path()).await? < cutoff
                && self.meta.get_upload(upload).await?.is_none()
            {
                fs::remove_dir_all(dir.path()).await?;
                report.uploads += 1;
            }
        }

        for f in entries(&disk.tmp_dir()).await? {
            let meta = f.metadata().await?;
            if meta.is_file() && meta.modified()? < cutoff {
                fs::remove_file(f.path()).await?;
                report.tmp += 1;
            }
        }
        Ok(())
    }

    // ---- rebalancing ----

    /// Moves copies from the fullest disks (least free space, as last reported by heartbeat)
    /// to the emptiest ones until their free space is within `margin`, keeping copies of one
    /// object on distinct nodes. Use it after adding disks or nodes; new writes already prefer
    /// emptier disks. Fails with [`StoreError::Busy`] while a repair or rebalance runs.
    pub async fn rebalance(&self, options: RebalanceOptions) -> Result<RebalanceReport> {
        let lease = self.lease(REPLICAS_LEASE).await?;
        self.refresh_cluster().await;
        let view = self.cluster_disks().await?;
        let records = self.meta.list_disks().await?;
        let single_node = view
            .iter()
            .map(|i| i.node_id.as_str())
            .collect::<HashSet<_>>()
            .len()
            <= 1;
        let mut usable: HashMap<DiskId, (DiskRef, String)> = HashMap::new();
        let mut free: HashMap<DiskId, i128> = HashMap::new();
        for info in view.iter().filter(|i| !i.draining) {
            if let (Some(r), Some(rec)) =
                (self.reach(info), records.iter().find(|r| r.id == info.id))
            {
                usable.insert(info.id, (r, info.node_id.clone()));
                free.insert(info.id, i128::from(rec.free_bytes));
            }
        }
        let margin = i128::from(options.margin);
        let mut report = RebalanceReport::default();
        let mut visited = HashSet::new();
        let spread = |free: &HashMap<DiskId, i128>, src: DiskId| {
            free.values().max().copied().unwrap_or(0) - free[&src]
        };

        'sources: while let Some(src) = free
            .iter()
            .filter(|(d, _)| !visited.contains(*d))
            .min_by_key(|(_, f)| **f)
            .map(|(d, _)| *d)
        {
            visited.insert(src);
            if spread(&free, src) <= margin {
                break;
            }
            let src_ref = usable[&src].0.clone();
            let mut after = None;
            loop {
                let ids = self.meta.list_disk_replicas(src, after, PAGE).await?;
                let Some(last) = ids.last() else { break };
                after = Some(*last);
                for id in ids {
                    lease.check()?;
                    if options.max_moves.is_some_and(|m| report.moved >= m) {
                        break 'sources;
                    }
                    if spread(&free, src) <= margin {
                        continue 'sources;
                    }
                    let Some(stored) = self.meta.get_object_by_id(id).await? else {
                        continue;
                    };
                    let size = i128::from(stored.object.size);
                    let other_nodes: HashSet<&str> = stored
                        .replicas
                        .iter()
                        .filter(|d| **d != src)
                        .filter_map(|d| view.iter().find(|i| i.id == *d))
                        .map(|i| i.node_id.as_str())
                        .collect();
                    let target = usable
                        .iter()
                        .filter(|(d, (_, node))| {
                            !stored.replicas.contains(d)
                                && (single_node || !other_nodes.contains(node.as_str()))
                                && free[*d] - free[&src] > margin.max(2 * size)
                        })
                        .max_by_key(|(d, _)| free[*d])
                        .map(|(d, (r, _))| (*d, r.clone()));
                    let Some((dst, dst_ref)) = target else {
                        continue;
                    };
                    match blob::copy_replica(
                        &src_ref,
                        &dst_ref,
                        id,
                        stored.object.size,
                        &stored.object.blake3,
                    )
                    .await
                    {
                        Ok(CopyOutcome::Copied) => {
                            if !self.meta.add_replica(id, dst).await? {
                                dst_ref.remove(Dest::Object(id)).await;
                                continue;
                            }
                            self.meta.remove_replica(id, src).await?;
                            src_ref.remove(Dest::Object(id)).await;
                            *free.get_mut(&src).expect("source is usable") += size;
                            *free.get_mut(&dst).expect("target is usable") -= size;
                            report.moved += 1;
                            report.bytes += stored.object.size;
                        }
                        Ok(CopyOutcome::SourceCorrupt) => {
                            tracing::warn!(object = %id, disk = %src_ref.label(), "corrupt copy found while rebalancing; dropping it");
                            self.meta.remove_replica(id, src).await?;
                            src_ref.remove(Dest::Object(id)).await;
                        }
                        Ok(CopyOutcome::SourceUnreadable) => {
                            tracing::warn!(object = %id, disk = %src_ref.label(), "cannot read copy while rebalancing");
                        }
                        Err(error) => {
                            tracing::warn!(object = %id, target = %dst_ref.label(), %error, "copy failed while rebalancing");
                        }
                    }
                }
            }
        }
        lease.release().await?;
        Ok(report)
    }
}

async fn has_files(objects: &Path) -> io::Result<bool> {
    for l1 in entries(objects).await? {
        for l2 in entries(&l1.path()).await? {
            if !entries(&l2.path()).await?.is_empty() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::retry_delay_ms;

    #[test]
    fn retry_delay_doubles_up_to_six_hours() {
        let min = 60 * 1000;
        assert_eq!(retry_delay_ms(0), 10 * min);
        assert_eq!(retry_delay_ms(1), 20 * min);
        assert_eq!(retry_delay_ms(4), 160 * min);
        assert_eq!(retry_delay_ms(5), 320 * min);
        assert_eq!(retry_delay_ms(6), 360 * min);
        assert_eq!(retry_delay_ms(u32::MAX), 360 * min);
    }
}
