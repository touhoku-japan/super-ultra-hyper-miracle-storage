//! Three nodes sharing one metadata database, connected by an in-process `Peer`.

mod common;

use std::{
    collections::{HashMap, HashSet},
    io,
    ops::Range,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use suhms_store::{
    BoxReader, CompletedPart, MIN_PART_SIZE, Peer, Peers, PutObjectRequest, ReplicaSink, Store,
    StoreConfig, TenantRecord,
    blob::{self, Dest, LocalSink},
    disk::DiskId,
};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use uuid::Uuid;

const OWNER: &str = "tenant-1";

/// Routes peer calls straight to the target node's local disks.
struct InProcPeer(Arc<Store>);

impl InProcPeer {
    fn disk(&self, id: DiskId) -> io::Result<&Arc<suhms_store::disk::Disk>> {
        self.0
            .local_disk(id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such disk"))
    }
}

#[async_trait]
impl Peer for InProcPeer {
    async fn open_sink(&self, disk: DiskId, dest: Dest) -> io::Result<Box<dyn ReplicaSink>> {
        Ok(Box::new(LocalSink::create(self.disk(disk)?, dest).await?))
    }
    async fn read(&self, disk: DiskId, b: Uuid, size: u64, r: Range<u64>) -> io::Result<BoxReader> {
        blob::read_local(self.disk(disk)?, b, size, r).await
    }
    async fn remove(&self, disk: DiskId, dest: Dest) -> io::Result<()> {
        blob::remove_path(&dest.path(self.disk(disk)?)).await;
        Ok(())
    }
    async fn remove_upload(&self, disk: DiskId, upload: Uuid) -> io::Result<()> {
        blob::remove_upload_dir(self.disk(disk)?, upload).await;
        Ok(())
    }
    async fn concat(
        &self,
        disk: DiskId,
        upload: Uuid,
        parts: Vec<(Uuid, String)>,
        object: Uuid,
    ) -> io::Result<String> {
        blob::concat_parts(self.disk(disk)?, upload, &parts, object).await
    }
}

/// A node behind a network partition: every call fails, but it keeps heartbeating to the
/// metadata database.
struct CutPeer;

fn cut() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "network partition")
}

#[async_trait]
impl Peer for CutPeer {
    async fn open_sink(&self, _: DiskId, _: Dest) -> io::Result<Box<dyn ReplicaSink>> {
        Err(cut())
    }
    async fn read(&self, _: DiskId, _: Uuid, _: u64, _: Range<u64>) -> io::Result<BoxReader> {
        Err(cut())
    }
    async fn remove(&self, _: DiskId, _: Dest) -> io::Result<()> {
        Err(cut())
    }
    async fn remove_upload(&self, _: DiskId, _: Uuid) -> io::Result<()> {
        Err(cut())
    }
    async fn concat(
        &self,
        _: DiskId,
        _: Uuid,
        _: Vec<(Uuid, String)>,
        _: Uuid,
    ) -> io::Result<String> {
        Err(cut())
    }
    async fn ping(&self) -> io::Result<()> {
        Err(cut())
    }
}

#[derive(Default)]
struct Registry {
    nodes: RwLock<HashMap<String, Arc<Store>>>,
    down: RwLock<HashSet<String>>,
    partitioned: RwLock<HashSet<String>>,
}

impl Peers for Registry {
    fn get(&self, node_id: &str) -> Option<Arc<dyn Peer>> {
        if self.down.read().unwrap().contains(node_id) {
            return None;
        }
        if self.partitioned.read().unwrap().contains(node_id) {
            return Some(Arc::new(CutPeer));
        }
        let store = self.nodes.read().unwrap().get(node_id).cloned()?;
        Some(Arc::new(InProcPeer(store)))
    }
}

struct Cluster {
    _dir: TempDir,
    registry: Arc<Registry>,
    nodes: Vec<Arc<Store>>,
    disk_node: HashMap<DiskId, String>,
    node_disks: HashMap<String, Vec<PathBuf>>,
}

impl Cluster {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Registry::default());
        let meta = common::metadata_url(dir.path()).await;
        let mut nodes = Vec::new();
        let mut node_disks = HashMap::new();
        for n in 1..=3 {
            let id = format!("node{n}");
            let disks: Vec<PathBuf> = (1..=2)
                .map(|d| {
                    let p = dir.path().join(format!("{id}-disk{d}"));
                    std::fs::create_dir(&p).unwrap();
                    p
                })
                .collect();
            let config = StoreConfig {
                node_id: id.clone(),
                disks: disks.clone(),
                metadata_url: meta.clone(),
                ..StoreConfig::default()
            };
            let store = Arc::new(
                Store::open_with_peers(config, registry.clone())
                    .await
                    .unwrap(),
            );
            registry
                .nodes
                .write()
                .unwrap()
                .insert(id.clone(), store.clone());
            node_disks.insert(id, disks);
            nodes.push(store);
        }
        nodes[0]
            .metadata()
            .create_tenant(&TenantRecord {
                id: OWNER.into(),
                name: OWNER.into(),
                ..TenantRecord::default()
            })
            .await
            .unwrap();
        let disk_node = nodes[0]
            .cluster_disks()
            .await
            .unwrap()
            .iter()
            .map(|d| (d.id, d.node_id.clone()))
            .collect();
        Self {
            _dir: dir,
            registry,
            nodes,
            disk_node,
            node_disks,
        }
    }

    async fn refresh(&self) {
        for n in &self.nodes {
            n.refresh_cluster().await;
        }
    }

    /// Simulates a crashed node: unreachable, and its heartbeats are stale.
    async fn crash(&self, i: usize) {
        let node = &self.nodes[i];
        self.registry
            .down
            .write()
            .unwrap()
            .insert(node.node_id().to_owned());
        for d in node.cluster_disks().await.unwrap().iter() {
            if d.node_id == node.node_id() {
                node.metadata().heartbeat_disk(d.id, 0, 0).await.unwrap();
            }
        }
        self.refresh().await;
    }

    async fn recover(&self, i: usize) {
        let node = &self.nodes[i];
        self.registry.down.write().unwrap().remove(node.node_id());
        node.heartbeat().await.unwrap();
        self.refresh().await;
    }

    async fn nodes_holding(&self, bucket: &str, key: &str) -> Vec<String> {
        let mut nodes: Vec<String> = self.nodes[0]
            .object_replicas(bucket, key)
            .await
            .unwrap()
            .iter()
            .map(|d| self.disk_node[d].clone())
            .collect();
        nodes.sort();
        nodes
    }

    /// Number of object files on a node's disks (plain files under objects/).
    fn files_on(&self, node: &str) -> usize {
        self.node_disks[node]
            .iter()
            .map(|d| walk(&d.join("objects")))
            .sum()
    }
}

fn walk(p: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(p) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            if e.path().is_dir() {
                walk(&e.path())
            } else {
                1
            }
        })
        .sum()
}

fn put(bucket: &str, key: &str) -> PutObjectRequest {
    PutObjectRequest {
        bucket: bucket.into(),
        key: key.into(),
        ..Default::default()
    }
}

async fn read(store: &Store, bucket: &str, key: &str) -> Vec<u8> {
    let stored = store.get_object(OWNER, bucket, key).await.unwrap();
    let mut r = store
        .read_object(&stored, 0..stored.object.size)
        .await
        .unwrap();
    let mut out = Vec::new();
    r.read_to_end(&mut out).await.unwrap();
    out
}

#[tokio::test]
async fn copies_land_on_three_nodes_and_are_readable_everywhere() {
    let c = Cluster::start().await;
    assert_eq!(c.nodes[0].max_replicas().await.unwrap(), 3);
    c.nodes[0].create_bucket(OWNER, "data", None).await.unwrap();
    let body: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();
    c.nodes[0]
        .put_object(OWNER, put("data", "obj"), &body[..])
        .await
        .unwrap();

    assert_eq!(
        c.nodes_holding("data", "obj").await,
        vec!["node1", "node2", "node3"]
    );
    for n in &c.nodes {
        assert_eq!(read(n, "data", "obj").await, body);
    }

    // Deleting through another node removes the files on every node.
    c.nodes[1]
        .delete_object(OWNER, "data", "obj")
        .await
        .unwrap();
    for node in ["node1", "node2", "node3"] {
        assert_eq!(c.files_on(node), 0, "{node}");
    }
}

#[tokio::test]
async fn node_outage_degrades_then_repair_restores() {
    let c = Cluster::start().await;
    c.nodes[0].create_bucket(OWNER, "data", None).await.unwrap();
    c.nodes[0]
        .put_object(OWNER, put("data", "before"), &b"before"[..])
        .await
        .unwrap();

    c.crash(2).await;
    // Reads are served from the surviving copies.
    assert_eq!(read(&c.nodes[1], "data", "before").await, b"before");
    // Writes still succeed with a majority (2 of 3), on the surviving nodes.
    c.nodes[0]
        .put_object(OWNER, put("data", "during"), &b"during"[..])
        .await
        .unwrap();
    assert_eq!(
        c.nodes_holding("data", "during").await,
        vec!["node1", "node2"]
    );
    // Nothing to copy to while node3 is down (both live nodes already hold every object).
    let r = c.nodes[0].repair().await.unwrap();
    assert_eq!(r.copied, 0);

    c.recover(2).await;
    let r = c.nodes[0].repair().await.unwrap();
    assert_eq!((r.copied, r.unrecoverable), (1, 0));
    assert_eq!(
        c.nodes_holding("data", "during").await,
        vec!["node1", "node2", "node3"]
    );
    assert_eq!(read(&c.nodes[2], "data", "during").await, b"during");
}

#[tokio::test]
async fn losing_a_node_for_good_rereplicates_elsewhere() {
    let c = Cluster::start().await;
    c.nodes[0]
        .create_bucket(OWNER, "data", Some(2))
        .await
        .unwrap();
    for k in ["a", "b", "c", "d"] {
        c.nodes[0]
            .put_object(OWNER, put("data", k), k.as_bytes())
            .await
            .unwrap();
    }
    // Pick a node that holds a copy of "a" and kill it.
    let victim = c.nodes_holding("data", "a").await[0].clone();
    let idx = c.nodes.iter().position(|n| n.node_id() == victim).unwrap();
    c.crash(idx).await;
    let survivor = &c.nodes[(idx + 1) % 3];
    let r = survivor.repair().await.unwrap();
    assert!(r.copied >= 1, "{r:?}");
    for k in ["a", "b", "c", "d"] {
        let live: Vec<_> = c
            .nodes_holding("data", k)
            .await
            .into_iter()
            .filter(|n| *n != victim)
            .collect();
        assert_eq!(live.len(), 2, "{k} has two live copies again");
        assert_eq!(read(survivor, "data", k).await, k.as_bytes());
    }
}

#[tokio::test]
async fn multipart_parts_from_different_nodes_assemble_on_every_node() {
    let c = Cluster::start().await;
    c.nodes[0].create_bucket(OWNER, "mpu", None).await.unwrap();
    let upload = c.nodes[0]
        .create_multipart_upload(OWNER, put("mpu", "big"))
        .await
        .unwrap();
    let p1: Vec<u8> = vec![1; MIN_PART_SIZE as usize];
    let p2: Vec<u8> = vec![2; 1000];
    // Parts arrive at different nodes, as behind a load balancer.
    let r1 = c.nodes[1]
        .upload_part(OWNER, "mpu", "big", upload.id, 1, None, &p1[..])
        .await
        .unwrap();
    let r2 = c.nodes[2]
        .upload_part(OWNER, "mpu", "big", upload.id, 2, None, &p2[..])
        .await
        .unwrap();
    let parts = [
        CompletedPart {
            part_number: 1,
            etag: r1.etag,
        },
        CompletedPart {
            part_number: 2,
            etag: r2.etag,
        },
    ];
    c.nodes[0]
        .complete_multipart_upload(OWNER, "mpu", "big", upload.id, &parts)
        .await
        .unwrap();
    assert_eq!(
        c.nodes_holding("mpu", "big").await,
        vec!["node1", "node2", "node3"]
    );
    let mut expected = p1;
    expected.extend_from_slice(&p2);
    for n in &c.nodes {
        assert_eq!(read(n, "mpu", "big").await, expected);
    }
}

#[tokio::test]
async fn draining_a_node_moves_its_copies_to_the_other_nodes() {
    let c = Cluster::start().await;
    c.nodes[0]
        .create_bucket(OWNER, "data", Some(2))
        .await
        .unwrap();
    let keys = ["a", "b", "c", "d", "e", "f"];
    for k in keys {
        c.nodes[0]
            .put_object(OWNER, put("data", k), k.as_bytes())
            .await
            .unwrap();
    }
    let victim = c.nodes_holding("data", "a").await[0].clone();
    for (disk, node) in &c.disk_node {
        if *node == victim {
            assert!(c.nodes[0].set_disk_draining(*disk, true).await.unwrap());
        }
    }
    c.refresh().await;
    // Any node may run the repair.
    let r = c.nodes[1].repair().await.unwrap();
    assert!(r.drained > 0, "{r:?}");
    assert_eq!(c.files_on(&victim), 0);
    for k in keys {
        let nodes = c.nodes_holding("data", k).await;
        assert_eq!(nodes.len(), 2, "{k}: {nodes:?}");
        assert_ne!(nodes[0], nodes[1]);
        assert!(!nodes.contains(&victim));
        assert_eq!(read(&c.nodes[0], "data", k).await, k.as_bytes());
    }
    for (disk, node) in &c.disk_node {
        if *node == victim {
            assert_eq!(
                c.nodes[0].remove_disk(*disk, false).await.unwrap(),
                suhms_store::RemoveDiskOutcome::Removed { dropped: 0 }
            );
        }
    }
}

#[tokio::test]
async fn a_node_that_comes_back_gets_its_surplus_copies_trimmed() {
    let c = Cluster::start().await;
    c.nodes[0]
        .create_bucket(OWNER, "data", Some(2))
        .await
        .unwrap();
    for k in ["a", "b", "c", "d"] {
        c.nodes[0]
            .put_object(OWNER, put("data", k), k.as_bytes())
            .await
            .unwrap();
    }
    let victim = c.nodes_holding("data", "a").await[0].clone();
    let idx = c.nodes.iter().position(|n| n.node_id() == victim).unwrap();
    c.crash(idx).await;
    let survivor = &c.nodes[(idx + 1) % 3];
    let r = survivor.repair().await.unwrap();
    assert!(r.queued > 0 && r.copied > 0, "{r:?}");

    c.recover(idx).await;
    let r = survivor.repair().await.unwrap();
    assert!(r.trimmed > 0, "{r:?}");
    for k in ["a", "b", "c", "d"] {
        let nodes = c.nodes_holding("data", k).await;
        assert_eq!(nodes.len(), 2, "{k}: {nodes:?}");
        assert_ne!(nodes[0], nodes[1]);
        assert_eq!(read(survivor, "data", k).await, k.as_bytes());
    }
    // Settled: another pass has nothing to do.
    assert_eq!(
        survivor.repair().await.unwrap(),
        suhms_store::RepairReport::default()
    );
}

/// Live copies of an object: its replica records on disks that are currently alive.
async fn live_nodes(c: &Cluster, bucket: &str, key: &str) -> Vec<String> {
    let view = c.nodes[0].cluster_disks().await.unwrap();
    let mut nodes: Vec<String> = c.nodes[0]
        .object_replicas(bucket, key)
        .await
        .unwrap()
        .iter()
        .filter(|d| view.iter().any(|i| i.id == **d && i.alive))
        .map(|d| c.disk_node[d].clone())
        .collect();
    nodes.sort();
    nodes
}

#[tokio::test]
async fn a_failed_disk_is_replaced_on_another_disk_of_the_same_node() {
    let c = Cluster::start().await;
    c.nodes[0].create_bucket(OWNER, "data", None).await.unwrap();
    let keys: Vec<String> = (0..8).map(|i| format!("k{i}")).collect();
    for k in &keys {
        c.nodes[0]
            .put_object(OWNER, put("data", k), k.as_bytes())
            .await
            .unwrap();
    }

    // node1 loses one disk (unmounted / pulled): its root disappears while the server runs.
    let root = c.node_disks["node1"][0].clone();
    let away = root.with_extension("away");
    std::fs::rename(&root, &away).unwrap();
    c.nodes[0].heartbeat().await.unwrap();
    // Its last heartbeat ages out (as 30 s without one would).
    let failed = c.nodes[0]
        .cluster_disks()
        .await
        .unwrap()
        .iter()
        .find(|d| d.node_id == "node1" && !d.alive)
        .expect("the failed disk is reported down at once on its own node")
        .id;
    c.nodes[0]
        .metadata()
        .heartbeat_disk(failed, 0, 0)
        .await
        .unwrap();
    c.refresh().await;

    // Scrub on node1 skips the missing disk instead of dropping every copy on it.
    let r = c.nodes[0].scrub().await.unwrap();
    assert_eq!((r.removed, r.skipped_disks), (0, 1), "{r:?}");

    // Every object is back to three live copies on three nodes: the copies that were on the
    // failed disk move to node1's other disk.
    let r = c.nodes[1].repair().await.unwrap();
    assert!(r.copied > 0 && r.unrecoverable == 0, "{r:?}");
    for k in &keys {
        assert_eq!(
            live_nodes(&c, "data", k).await,
            vec!["node1", "node2", "node3"],
            "{k}"
        );
        assert_eq!(read(&c.nodes[0], "data", k).await, k.as_bytes());
    }
    // Nothing was written to where the disk used to be mounted.
    assert!(!root.exists());

    // The disk comes back: the extra copies are trimmed, never leaving two on one node.
    std::fs::rename(&away, &root).unwrap();
    c.nodes[0].heartbeat().await.unwrap();
    c.refresh().await;
    let r = c.nodes[1].repair().await.unwrap();
    assert!(r.trimmed > 0, "{r:?}");
    for k in &keys {
        assert_eq!(
            c.nodes_holding("data", k).await,
            vec!["node1", "node2", "node3"],
            "{k}"
        );
        assert_eq!(read(&c.nodes[0], "data", k).await, k.as_bytes());
    }
    let r = c.nodes[0].scrub().await.unwrap();
    assert_eq!((r.removed, r.skipped_disks), (0, 0), "{r:?}");
}

#[tokio::test]
async fn repair_resumes_as_soon_as_a_partition_heals() {
    let c = Cluster::start().await;
    c.nodes[0].create_bucket(OWNER, "data", None).await.unwrap();

    // node3 is cut off from the other nodes but still heartbeats to the database.
    c.registry
        .partitioned
        .write()
        .unwrap()
        .insert("node3".into());
    c.nodes[0].heartbeat().await.unwrap();
    c.nodes[0]
        .put_object(OWNER, put("data", "during"), &b"during"[..])
        .await
        .unwrap();
    assert_eq!(
        c.nodes_holding("data", "during").await,
        vec!["node1", "node2"]
    );
    // Cannot be fixed now: the object waits (backoff) instead of being retried every pass.
    let r = c.nodes[0].repair().await.unwrap();
    assert_eq!(r.copied, 0, "{r:?}");

    // The partition heals. The next pass notices node3 answering again and does not wait
    // out the backoff.
    c.registry.partitioned.write().unwrap().remove("node3");
    let r = c.nodes[0].repair().await.unwrap();
    assert_eq!((r.copied, r.unrecoverable), (1, 0), "{r:?}");
    assert_eq!(
        c.nodes_holding("data", "during").await,
        vec!["node1", "node2", "node3"]
    );
}

#[tokio::test]
async fn a_heartbeat_brings_a_disk_marked_offline_back() {
    let c = Cluster::start().await;
    // An admin command on node3 that could not open a disk marks node3's disks offline.
    c.nodes[0]
        .metadata()
        .set_node_disks_offline("node3")
        .await
        .unwrap();
    c.refresh().await;
    let down = |v: &[suhms_store::DiskInfo]| {
        v.iter()
            .filter(|d| d.node_id == "node3" && !d.alive)
            .count()
    };
    assert_eq!(down(&c.nodes[0].cluster_disks().await.unwrap()), 2);
    // The running server's next heartbeat corrects it.
    c.nodes[2].heartbeat().await.unwrap();
    c.refresh().await;
    assert_eq!(down(&c.nodes[0].cluster_disks().await.unwrap()), 0);
}
