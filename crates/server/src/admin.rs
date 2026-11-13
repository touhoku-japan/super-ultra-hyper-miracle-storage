//! Administrative commands (`suhms tenant ...`, `suhms key ...`, ...).
//!
//! Tenant and key commands touch only the metadata database, so they are safe to run while the
//! server is up. Commands that open the disks never clear temp files, for the same reason.

use std::sync::Arc;

use anyhow::{Context, bail};
use clap::Subcommand;
use suhms_store::{
    AccessKeyRecord,
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

pub async fn repair(config: Config) -> anyhow::Result<()> {
    let store = crate::cluster::open_store(&config).await?;
    let r = store.repair().await?;
    println!(
        "copied {}, trimmed {}, unrecoverable {}",
        r.copied, r.trimmed, r.unrecoverable
    );
    if r.unrecoverable > 0 {
        bail!(
            "{} objects have no healthy copy on an online disk",
            r.unrecoverable
        );
    }
    Ok(())
}
