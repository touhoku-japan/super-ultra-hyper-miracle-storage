//! Administrative commands (`suhms tenant ...`, `suhms key ...`, ...).
//!
//! Tenant and key commands touch only the metadata database, so they are safe to run while the
//! server is up. Commands that open the disks never clear temp files, for the same reason.

use std::sync::Arc;

use anyhow::{Context, bail};
use clap::Subcommand;
use suhms_store::{
    AccessKeyRecord, DiskRecord, GcOptions, RebalanceOptions, RemoveDiskOutcome,
    disk::DiskId,
    meta::{self, MetadataStore},
};

use crate::{
    config::Config,
    crypto::{self, SecretCipher},
};

#[derive(Debug, Subcommand)]
pub enum TenantCmd {
    /// Create a tenant (customer account)
    Create { name: String },
    /// List tenants
    List,
}

#[derive(Debug, Subcommand)]
pub enum KeyCmd {
    /// Issue a new access key for a tenant and print its secret (shown only once)
    Create { tenant: String },
    /// List a tenant's access keys
    List { tenant: String },
    /// Disable an access key (requests signed with it are rejected)
    Disable { key_id: String },
    /// Re-enable a disabled access key
    Enable { key_id: String },
}

#[derive(Debug, Subcommand)]
pub enum BucketCmd {
    /// Set a bucket's replica count (a number, or "default" for the cluster default);
    /// run `suhms repair` afterwards to add or remove copies of existing objects
    SetReplicas { bucket: String, replicas: String },
}

#[derive(Debug, Subcommand)]
pub enum DiskCmd {
    /// List every disk of the cluster
    List,
    /// Stop placing new copies on a disk and have repair move its copies elsewhere
    Drain { disk: String },
    /// Cancel draining
    Undrain { disk: String },
    /// Forget a drained disk (remove it from the node's config first). With --force, also for
    /// a disk that died with copies on it: their records are dropped and repair re-replicates
    Remove {
        disk: String,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum NodeCmd {
    /// Drain every disk of a node (decommissioning it)
    Drain { node: String },
    /// Cancel draining a node
    Undrain { node: String },
    /// Forget every disk of a drained node (see `disk remove`)
    Remove {
        node: String,
        #[arg(long)]
        force: bool,
    },
}

pub fn now_millis() -> i64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is after 1970");
    i64::try_from(d.as_millis()).expect("timestamp fits in i64")
}

async fn metadata(config: &Config) -> anyhow::Result<Arc<dyn MetadataStore>> {
    Ok(meta::connect(&config.storage.metadata_url).await?)
}

async fn tenant_id(meta: &dyn MetadataStore, name: &str) -> anyhow::Result<String> {
    Ok(meta
        .get_tenant_by_name(name)
        .await?
        .with_context(|| format!("tenant {name} does not exist"))?
        .id)
}

/// Creates a key for the tenant and returns (key id, secret).
pub async fn issue_key(
    meta: &dyn MetadataStore,
    cipher: &SecretCipher,
    tenant_id: &str,
) -> anyhow::Result<(String, String)> {
    let id = crypto::generate_access_key_id();
    let secret = crypto::generate_secret_key();
    meta.create_access_key(&AccessKeyRecord {
        id: id.clone(),
        tenant_id: tenant_id.to_owned(),
        secret_enc: cipher.encrypt(&id, &secret),
        active: true,
        created_at: now_millis(),
    })
    .await?;
    Ok((id, secret))
}

pub async fn tenant(config: &Config, cmd: TenantCmd) -> anyhow::Result<()> {
    let meta = metadata(config).await?;
    match cmd {
        TenantCmd::Create { name } => {
            if name.is_empty() || name.len() > 128 {
                bail!("tenant name must be 1-128 bytes");
            }
            let record = suhms_store::TenantRecord {
                id: uuid::Uuid::new_v4().to_string(),
                name: name.clone(),
                created_at: now_millis(),
                ..suhms_store::TenantRecord::default()
            };
            if !meta.create_tenant(&record).await? {
                bail!("tenant {name} already exists");
            }
            println!("created tenant {name} (id {})", record.id);
        }
        TenantCmd::List => {
            for t in meta.list_tenants().await? {
                println!("{}\t{}", t.id, t.name);
            }
        }
    }
    Ok(())
}

pub async fn key(config: &Config, cmd: KeyCmd) -> anyhow::Result<()> {
    let meta = metadata(config).await?;
    match cmd {
        KeyCmd::Create { tenant } => {
            let cipher = SecretCipher::load(&config.auth)?;
            let tid = tenant_id(meta.as_ref(), &tenant).await?;
            let (id, secret) = issue_key(meta.as_ref(), &cipher, &tid).await?;
            println!("access_key_id     = {id}");
            println!("secret_access_key = {secret}");
            eprintln!("(the secret is not shown again)");
        }
        KeyCmd::List { tenant } => {
            let tid = tenant_id(meta.as_ref(), &tenant).await?;
            for k in meta.list_access_keys(&tid).await? {
                let state = if k.active { "active" } else { "disabled" };
                println!("{}\t{state}", k.id);
            }
        }
        KeyCmd::Disable { key_id } => set_active(meta.as_ref(), &key_id, false).await?,
        KeyCmd::Enable { key_id } => set_active(meta.as_ref(), &key_id, true).await?,
    }
    Ok(())
}

async fn set_active(meta: &dyn MetadataStore, key_id: &str, active: bool) -> anyhow::Result<()> {
    if !meta.set_access_key_active(key_id, active).await? {
        bail!("access key {key_id} does not exist");
    }
    println!("{key_id} {}", if active { "enabled" } else { "disabled" });
    Ok(())
}

pub async fn bucket(config: Config, cmd: BucketCmd) -> anyhow::Result<()> {
    let store = crate::cluster::open_store(&config).await?;
    match cmd {
        BucketCmd::SetReplicas { bucket, replicas } => {
            let n = match replicas.as_str() {
                "default" => None,
                n => Some(
                    n.parse::<u8>()
                        .context("replicas must be a number or \"default\"")?,
                ),
            };
            store.set_bucket_replicas(&bucket, n).await?;
            println!("bucket {bucket}: replicas = {replicas}; run `suhms repair` to apply");
        }
    }
    Ok(())
}

pub async fn scrub(config: Config) -> anyhow::Result<()> {
    let store = crate::cluster::open_store(&config).await?;
    let r = store.scrub().await?;
    println!(
        "checked {} copies, removed {} bad copies",
        r.checked, r.removed
    );
    Ok(())
}

pub async fn repair(config: Config, full: bool) -> anyhow::Result<()> {
    let store = crate::cluster::open_store(&config).await?;
    let r = if full {
        store.repair_full().await?
    } else {
        store.repair().await?
    };
    println!(
        "copied {}, trimmed {}, drained {}, queued {}, unrecoverable {}",
        r.copied, r.trimmed, r.drained, r.queued, r.unrecoverable
    );
    if r.unrecoverable > 0 {
        bail!(
            "{} objects have no healthy copy on an online disk",
            r.unrecoverable
        );
    }
    Ok(())
}

pub async fn gc(config: Config, grace_secs: Option<u64>, force: bool) -> anyhow::Result<()> {
    let store = crate::cluster::open_store(&config).await?;
    let grace = grace_secs.unwrap_or(config.maintenance.gc_grace_secs);
    let r = store
        .gc(GcOptions {
            grace: std::time::Duration::from_secs(grace),
            force,
        })
        .await?;
    println!(
        "orphaned files moved to trash: {} ({} bytes), stale upload dirs: {}, temp files: {}, trash dirs purged: {}",
        r.orphans, r.orphan_bytes, r.uploads, r.tmp, r.purged
    );
    if r.skipped_disks > 0 {
        bail!(
            "{} disks skipped: the metadata records no copies on them (wrong metadata database?); use --force if that is expected",
            r.skipped_disks
        );
    }
    Ok(())
}

pub async fn rebalance(
    config: Config,
    margin_gib: u64,
    max_moves: Option<u64>,
) -> anyhow::Result<()> {
    let store = crate::cluster::open_store(&config).await?;
    let r = store
        .rebalance(RebalanceOptions {
            margin: margin_gib << 30,
            max_moves,
        })
        .await?;
    println!("moved {} copies ({} bytes)", r.moved, r.bytes);
    Ok(())
}

fn parse_disk(s: &str) -> anyhow::Result<DiskId> {
    DiskId::try_parse(s).with_context(|| format!("{s} is not a disk id (see `suhms disk list`)"))
}

/// A disk counts as up when its node heartbeated recently (as the server judges it).
fn is_up(d: &DiskRecord) -> bool {
    d.online && d.heartbeat_at >= now_millis() - 30_000
}

/// Starts or stops draining disks and queues their objects for repair.
async fn set_draining(
    config: &Config,
    meta: &dyn MetadataStore,
    disks: &[DiskId],
    draining: bool,
) -> anyhow::Result<()> {
    for disk in disks {
        anyhow::ensure!(
            meta.set_disk_draining(*disk, draining).await?,
            "disk {disk} does not exist"
        );
        let queued = meta.enqueue_disk_repairs(*disk).await?;
        println!(
            "disk {disk}: {} ({queued} objects queued for repair)",
            if draining { "draining" } else { "active" }
        );
    }
    if draining {
        warn_capacity(config, &meta.list_disks().await?);
    }
    Ok(())
}

/// Draining only finishes if the remaining disks can hold every object's copies.
fn warn_capacity(config: &Config, disks: &[DiskRecord]) {
    let active: Vec<&DiskRecord> = disks.iter().filter(|d| !d.draining).collect();
    let all_nodes: std::collections::HashSet<&str> =
        disks.iter().map(|d| d.node_id.as_str()).collect();
    let active_nodes: std::collections::HashSet<&str> =
        active.iter().map(|d| d.node_id.as_str()).collect();
    let room = if all_nodes.len() > 1 {
        active_nodes.len()
    } else {
        active.len()
    };
    let needed = config
        .storage
        .storage_classes
        .values()
        .copied()
        .chain([config.storage.default_replicas])
        .max()
        .unwrap_or(1);
    if room < usize::from(needed) {
        println!(
            "warning: only {room} {} remain for copies, but objects may need {needed}; \
             objects with more copies than that cannot leave the draining disks until you add capacity",
            if all_nodes.len() > 1 {
                "nodes"
            } else {
                "disks"
            }
        );
    }
}

async fn remove_disks(
    meta: &dyn MetadataStore,
    disks: &[DiskId],
    force: bool,
) -> anyhow::Result<()> {
    let mut blocked = 0;
    for disk in disks {
        match meta.remove_disk(*disk, force).await? {
            RemoveDiskOutcome::Removed { dropped: 0 } => println!("disk {disk}: removed"),
            RemoveDiskOutcome::Removed { dropped } => println!(
                "disk {disk}: removed; dropped {dropped} copy records and queued those objects for repair"
            ),
            RemoveDiskOutcome::NotFound => bail!("disk {disk} does not exist"),
            RemoveDiskOutcome::HasReplicas(n) => {
                println!(
                    "disk {disk}: still holds {n} copies; drain it and let repair finish (or use --force if the disk is gone)"
                );
                blocked += 1;
            }
        }
    }
    if blocked > 0 {
        bail!("{blocked} disks were not removed");
    }
    Ok(())
}

pub async fn disk(config: &Config, cmd: DiskCmd) -> anyhow::Result<()> {
    let meta = metadata(config).await?;
    match cmd {
        DiskCmd::List => {
            println!(
                "{:<36}  {:<12}  {:<9}  {:>10}  {:>10}  path",
                "id", "node", "state", "free GiB", "copies"
            );
            for d in meta.list_disks().await? {
                let state = match (is_up(&d), d.draining) {
                    (true, false) => "up",
                    (true, true) => "draining",
                    (false, false) => "down",
                    (false, true) => "down,drain",
                };
                println!(
                    "{:<36}  {:<12}  {:<9}  {:>10}  {:>10}  {}",
                    d.id,
                    d.node_id,
                    state,
                    d.free_bytes >> 30,
                    meta.count_disk_replicas(d.id).await?,
                    d.path
                );
            }
        }
        DiskCmd::Drain { disk } => {
            set_draining(config, &*meta, &[parse_disk(&disk)?], true).await?
        }
        DiskCmd::Undrain { disk } => {
            set_draining(config, &*meta, &[parse_disk(&disk)?], false).await?;
        }
        DiskCmd::Remove { disk, force } => {
            remove_disks(&*meta, &[parse_disk(&disk)?], force).await?;
        }
    }
    Ok(())
}

pub async fn node(config: &Config, cmd: NodeCmd) -> anyhow::Result<()> {
    let meta = metadata(config).await?;
    let disks_of = |node: &str, all: &[DiskRecord]| -> anyhow::Result<Vec<DiskId>> {
        let ids: Vec<DiskId> = all
            .iter()
            .filter(|d| d.node_id == node)
            .map(|d| d.id)
            .collect();
        anyhow::ensure!(!ids.is_empty(), "node {node} has no disks");
        Ok(ids)
    };
    let all = meta.list_disks().await?;
    match cmd {
        NodeCmd::Drain { node } => {
            set_draining(config, &*meta, &disks_of(&node, &all)?, true).await?
        }
        NodeCmd::Undrain { node } => {
            set_draining(config, &*meta, &disks_of(&node, &all)?, false).await?;
        }
        NodeCmd::Remove { node, force } => {
            remove_disks(&*meta, &disks_of(&node, &all)?, force).await?;
        }
    }
    Ok(())
}
