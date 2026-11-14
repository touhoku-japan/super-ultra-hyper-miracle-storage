//! Operations on one node: garbage collection, draining and removing disks, the repair lease,
//! and rebalancing.

mod common;

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use suhms_store::{
    GcOptions, PutObjectRequest, RebalanceOptions, RemoveDiskOutcome, Store, StoreConfig,
    StoreError, TenantRecord, disk::DiskId,
};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use uuid::Uuid;

const OWNER: &str = "tenant-1";
const HOUR: Duration = Duration::from_secs(3600);

struct Env {
    _dir: TempDir,
    disks: Vec<PathBuf>,
    store: Store,
}

impl Env {
    async fn new(disks: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let disks: Vec<PathBuf> = (0..disks)
            .map(|i| {
                let p = dir.path().join(format!("disk{i}"));
                std::fs::create_dir(&p).unwrap();
                p
            })
            .collect();
        let meta_url = common::metadata_url(dir.path()).await;
        let store = Store::open(StoreConfig {
            disks: disks.clone(),
            metadata_url: meta_url,
            ..StoreConfig::default()
        })
        .await
        .unwrap();
        store
            .metadata()
            .create_tenant(&TenantRecord {
                id: OWNER.into(),
                name: OWNER.into(),
                ..TenantRecord::default()
            })
            .await
            .unwrap();
        Self {
            _dir: dir,
            disks,
            store,
        }
    }

    /// Disk id of the i-th configured disk.
    async fn disk_id(&self, i: usize) -> DiskId {
        self.store
            .cluster_disks()
            .await
            .unwrap()
            .iter()
            .map(|d| d.id)
            .find(|id| self.store.local_disk(*id).unwrap().root == self.disks[i])
            .unwrap()
    }

    async fn put(&self, bucket: &str, key: &str, class: Option<&str>) {
        self.store
            .put_object(
                OWNER,
                PutObjectRequest {
                    bucket: bucket.into(),
                    key: key.into(),
                    storage_class: class.map(Into::into),
                    ..Default::default()
                },
                key.as_bytes(),
            )
            .await
            .unwrap();
    }

    async fn read(&self, bucket: &str, key: &str) -> Vec<u8> {
        let stored = self.store.get_object(OWNER, bucket, key).await.unwrap();
        let mut r = self
            .store
            .read_object(&stored, 0..stored.object.size)
            .await
            .unwrap();
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.unwrap();
        out
    }

    async fn copies_on(&self, disk: DiskId) -> u64 {
        self.store
            .metadata()
            .count_disk_replicas(disk)
            .await
            .unwrap()
    }
}

fn backdate(path: &Path, by: Duration) {
    let f = std::fs::File::open(path).unwrap();
    f.set_modified(SystemTime::now() - by).unwrap();
}

/// Creates a file where an object with `id` would live on `disk`, `age` old.
fn plant(disk: &Path, id: Uuid, age: Duration) -> PathBuf {
    let name = id.simple().to_string();
    let dir = disk.join("objects").join(&name[0..2]).join(&name[2..4]);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(&name);
    std::fs::write(&path, b"orphan").unwrap();
    backdate(&path, age);
    path
}

fn count_files(p: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(p) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            if e.path().is_dir() {
                count_files(&e.path())
            } else {
                1
            }
        })
        .sum()
}

#[tokio::test]
async fn gc_collects_orphans_and_leaves_everything_else() {
    let env = Env::new(3).await;
    env.store.create_bucket(OWNER, "gcb", None).await.unwrap();
    env.put("gcb", "kept", None).await;
    let kept = env.store.object_locations("gcb", "kept").await.unwrap();
    for p in &kept {
        backdate(p, 2 * HOUR);
    }

    let orphan = plant(&env.disks[0], Uuid::new_v4(), 2 * HOUR);
    let fresh = plant(&env.disks[1], Uuid::new_v4(), Duration::ZERO);
    let stale_upload = env.disks[2]
        .join(".suhms/multipart")
        .join(Uuid::new_v4().simple().to_string());
    std::fs::create_dir_all(&stale_upload).unwrap();
    std::fs::write(stale_upload.join("part"), b"x").unwrap();
    backdate(&stale_upload, 2 * HOUR);
    let tmp = env.disks[0].join(".suhms/tmp/leftover");
    std::fs::write(&tmp, b"x").unwrap();
    backdate(&tmp, 2 * HOUR);

    let options = GcOptions {
        grace: HOUR,
        force: false,
    };
    let r = env.store.gc(options).await.unwrap();
    assert_eq!(
        (r.orphans, r.uploads, r.tmp, r.skipped_disks),
        (1, 1, 1, 0),
        "{r:?}"
    );
    assert!(!orphan.exists() && !stale_upload.exists() && !tmp.exists());
    assert!(fresh.exists(), "files younger than the grace period stay");
    assert!(kept.iter().all(|p| p.exists()));
    assert_eq!(env.read("gcb", "kept").await, b"kept");

    // The orphan sits in the trash until a later run finds it older than the grace period.
    let trash = env.disks[0].join(".suhms/gc");
    assert_eq!(count_files(&trash), 1);
    let run = std::fs::read_dir(&trash)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::rename(&run, trash.join("1000")).unwrap();
    let r = env.store.gc(options).await.unwrap();
    assert_eq!(r.purged, 1);
    assert_eq!(count_files(&trash), 0);
}

#[tokio::test]
async fn gc_skips_a_disk_the_metadata_knows_nothing_about() {
    let env = Env::new(2).await;
    let orphan = plant(&env.disks[0], Uuid::new_v4(), 2 * HOUR);
    let mut options = GcOptions {
        grace: HOUR,
        force: false,
    };
    let r = env.store.gc(options).await.unwrap();
    assert_eq!((r.orphans, r.skipped_disks), (0, 1));
    assert!(orphan.exists());

    options.force = true;
    let r = env.store.gc(options).await.unwrap();
    assert_eq!((r.orphans, r.skipped_disks), (1, 0));
}

#[tokio::test]
async fn draining_a_disk_moves_its_copies_then_it_can_be_removed() {
    let env = Env::new(4).await;
    env.store.create_bucket(OWNER, "drain", None).await.unwrap();
    let keys = ["a", "b", "c", "d", "e"];
    for k in keys {
        env.put("drain", k, None).await;
    }
    let d0 = env.disk_id(0).await;
    let on_d0 = env.copies_on(d0).await;
    assert!(on_d0 > 0);

    assert!(env.store.set_disk_draining(d0, true).await.unwrap());
    assert_eq!(
        env.store.remove_disk(d0, false).await.unwrap(),
        RemoveDiskOutcome::HasReplicas(on_d0)
    );
    // New writes avoid the draining disk.
    env.put("drain", "new", None).await;
    assert!(
        !env.store
            .object_replicas("drain", "new")
            .await
            .unwrap()
            .contains(&d0)
    );

    let r = env.store.repair().await.unwrap();
    assert_eq!((r.copied, r.drained), (on_d0, on_d0), "{r:?}");
    assert_eq!(env.copies_on(d0).await, 0);
    assert_eq!(count_files(&env.disks[0].join("objects")), 0);
    for k in keys {
        let replicas = env.store.object_replicas("drain", k).await.unwrap();
        assert_eq!(replicas.len(), 3);
        assert!(!replicas.contains(&d0));
        assert_eq!(env.read("drain", k).await, k.as_bytes());
    }

    assert_eq!(
        env.store.remove_disk(d0, false).await.unwrap(),
        RemoveDiskOutcome::Removed { dropped: 0 }
    );
    assert_eq!(env.store.cluster_disks().await.unwrap().len(), 3);
    // Nothing left to do.
    assert_eq!(env.store.repair().await.unwrap().copied, 0);
}

#[tokio::test]
async fn force_removing_a_dead_disk_rereplicates_its_objects() {
    let env = Env::new(4).await;
    env.store.create_bucket(OWNER, "dead", None).await.unwrap();
    for k in ["a", "b", "c"] {
        env.put("dead", k, None).await;
    }
    let d1 = env.disk_id(1).await;
    let lost = env.copies_on(d1).await;
    assert_eq!(
        env.store.remove_disk(d1, true).await.unwrap(),
        RemoveDiskOutcome::Removed { dropped: lost }
    );
    let r = env.store.repair().await.unwrap();
    assert_eq!(r.copied, lost, "{r:?}");
    for k in ["a", "b", "c"] {
        let replicas = env.store.object_replicas("dead", k).await.unwrap();
        assert_eq!(replicas.len(), 3);
        assert!(!replicas.contains(&d1));
    }
}

#[tokio::test]
async fn one_repair_at_a_time() {
    let env = Env::new(3).await;
    let meta = env.store.metadata();
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(
        meta.try_acquire_lease("replicas", "other-node", now, 60_000)
            .await
            .unwrap()
    );
    assert!(matches!(env.store.repair().await, Err(StoreError::Busy(_))));
    assert!(matches!(
        env.store.rebalance(RebalanceOptions::default()).await,
        Err(StoreError::Busy(_))
    ));
    // The holder can renew; others still cannot take it.
    assert!(
        meta.try_acquire_lease("replicas", "other-node", now + 1, 60_000)
            .await
            .unwrap()
    );
    assert!(
        !meta
            .try_acquire_lease("replicas", "third", now + 2, 60_000)
            .await
            .unwrap()
    );

    meta.release_lease("replicas", "other-node").await.unwrap();
    env.store.repair().await.unwrap();
    // The repair gave its lease back.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        meta.try_acquire_lease("replicas", "third", now + 3, 60_000)
            .await
            .unwrap()
    );
    // An expired lease can be taken over.
    assert!(
        meta.try_acquire_lease("replicas", "fourth", now + 60_004, 60_000)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn rebalance_moves_copies_to_emptier_disks() {
    let env = Env::new(3).await;
    env.store.create_bucket(OWNER, "rbl", None).await.unwrap();
    let keys = ["a", "b", "c", "d", "e", "f"];
    for k in keys {
        env.put("rbl", k, Some("REDUCED_REDUNDANCY")).await;
    }
    let ids = [
        env.disk_id(0).await,
        env.disk_id(1).await,
        env.disk_id(2).await,
    ];
    // Pretend disk 0 is nearly full and the others empty (as reported by heartbeats).
    let meta = env.store.metadata();
    let full = {
        let mut best = ids[0];
        for id in ids {
            if env.copies_on(id).await > env.copies_on(best).await {
                best = id;
            }
        }
        best
    };
    let before = env.copies_on(full).await;
    assert!(before >= 2);
    for id in ids {
        let free = if id == full { 1 << 30 } else { 100 << 30 };
        meta.heartbeat_disk(id, free, 1).await.unwrap();
    }

    let limited = env
        .store
        .rebalance(RebalanceOptions {
            margin: 1 << 20,
            max_moves: Some(1),
        })
        .await
        .unwrap();
    assert_eq!(limited.moved, 1);
    let r = env
        .store
        .rebalance(RebalanceOptions {
            margin: 1 << 20,
            max_moves: None,
        })
        .await
        .unwrap();
    assert_eq!(r.moved + 1, before, "{r:?}");
    assert_eq!(env.copies_on(full).await, 0);
    for k in keys {
        assert_eq!(env.store.object_replicas("rbl", k).await.unwrap().len(), 1);
        assert_eq!(env.read("rbl", k).await, k.as_bytes());
    }
}

#[tokio::test]
async fn unrepairable_objects_wait_instead_of_being_retried_every_pass() {
    let env = Env::new(3).await;
    env.store.create_bucket(OWNER, "wait", None).await.unwrap();
    env.put("wait", "k", None).await;
    // With one of three disks draining, a third copy has nowhere to go.
    let d0 = env.disk_id(0).await;
    env.store.set_disk_draining(d0, true).await.unwrap();
    let r = env.store.repair().await.unwrap();
    assert_eq!((r.copied, r.postponed), (0, 1), "{r:?}");
    // Not due yet: the next pass does not look at it.
    let r = env.store.repair().await.unwrap();
    assert_eq!(r.postponed, 0, "{r:?}");
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let meta = env.store.metadata();
    assert!(
        meta.list_repair_queue(None, 10, now)
            .await
            .unwrap()
            .is_empty()
    );
    let later = meta
        .list_repair_queue(None, 10, now + 11 * 60 * 1000)
        .await
        .unwrap();
    assert_eq!(later.len(), 1);
    assert_eq!(later[0].attempts, 1);

    // Undraining queues the disk's objects again, which ends the wait.
    env.store.set_disk_draining(d0, false).await.unwrap();
    let r = env.store.repair().await.unwrap();
    assert_eq!(r.postponed, 0, "{r:?}");
    assert!(
        meta.list_repair_queue(None, 10, now + 11 * 60 * 1000)
            .await
            .unwrap()
            .is_empty(),
        "fully replicated objects leave the queue"
    );

    // New capacity also ends every wait.
    meta.enqueue_repair(
        env.store
            .get_object(OWNER, "wait", "k")
            .await
            .unwrap()
            .object
            .id,
    )
    .await
    .unwrap();
    let id = meta.list_repair_queue(None, 10, now).await.unwrap()[0].object_id;
    meta.postpone_repair(id, now + 3_600_000).await.unwrap();
    assert_eq!(meta.reset_repair_backoff().await.unwrap(), 1);
    assert_eq!(
        meta.list_repair_queue(None, 10, now).await.unwrap().len(),
        1
    );
}
