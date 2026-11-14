use std::{path::PathBuf, sync::Arc};

use clap::{Parser, Subcommand};
use suhms_server::{
    admin, app,
    config::{Config, LogFormat},
    crypto::SecretCipher,
};
use tokio::net::TcpListener;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
#[command(version, about = "Super-Ultra-Hyper-Miracle Storage server")]
struct Args {
    /// Path to the TOML config file (defaults are used when omitted)
    #[arg(short, long, env = "SUHMS_CONFIG", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the S3 server (the default)
    Serve,
    /// Print a new random master key for [auth]
    GenMasterKey,
    /// Manage tenants
    #[command(subcommand)]
    Tenant(admin::TenantCmd),
    /// Manage access keys
    #[command(subcommand)]
    Key(admin::KeyCmd),
    /// Manage buckets
    #[command(subcommand)]
    Bucket(admin::BucketCmd),
    /// Re-hash every copy on this node's disks and drop missing or corrupt ones
    Scrub,
    /// Bring every object to its desired number of copies (checks every object unless --quick)
    Repair {
        /// Only the repair queue (what the server does every repair_interval_secs)
        #[arg(long)]
        quick: bool,
    },
    /// Collect files on this node's disks that no metadata refers to
    Gc {
        /// Leave files younger than this alone (default: [maintenance] gc_grace_secs)
        #[arg(long)]
        grace_secs: Option<u64>,
        /// Also on disks for which the metadata records no copies at all
        #[arg(long)]
        force: bool,
    },
    /// Move copies from the fullest disks to the emptiest ones (after adding disks or nodes)
    Rebalance {
        /// Stop when free space differs by at most this many GiB
        #[arg(long, default_value_t = 10)]
        margin_gib: u64,
        /// Stop after moving this many copies
        #[arg(long)]
        max_moves: Option<u64>,
    },
    /// Add checksums (CRC64NVME) to objects stored before every upload got one
    ChecksumBackfill {
        /// Stop after this many objects
        #[arg(long)]
        max_objects: Option<u64>,
        /// Read at most this many MiB/s (default: unlimited)
        #[arg(long)]
        mib_per_sec: Option<u64>,
    },
    /// Manage disks (draining, removal)
    #[command(subcommand)]
    Disk(admin::DiskCmd),
    /// Drain or remove every disk of a node
    #[command(subcommand)]
    Node(admin::NodeCmd),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = Config::load(args.config.as_deref())?;
    init_tracing(&config);

    match args.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(config).await,
        Command::GenMasterKey => {
            println!("{}", SecretCipher::generate_hex());
            Ok(())
        }
        Command::Tenant(cmd) => admin::tenant(&config, cmd).await,
        Command::Key(cmd) => admin::key(&config, cmd).await,
        Command::Bucket(cmd) => admin::bucket(config, cmd).await,
        Command::Scrub => admin::scrub(config).await,
        Command::Repair { quick } => admin::repair(config, !quick).await,
        Command::Gc { grace_secs, force } => admin::gc(config, grace_secs, force).await,
        Command::Rebalance {
            margin_gib,
            max_moves,
        } => admin::rebalance(config, margin_gib, max_moves).await,
        Command::ChecksumBackfill {
            max_objects,
            mib_per_sec,
        } => admin::checksum_backfill(config, max_objects, mib_per_sec).await,
        Command::Disk(cmd) => admin::disk(&config, cmd).await,
        Command::Node(cmd) => admin::node(&config, cmd).await,
    }
}

async fn serve(config: Config) -> anyhow::Result<()> {
    let cipher = Arc::new(SecretCipher::load(&config.auth)?);
    let store = Arc::new(suhms_server::cluster::open_store(&config).await?);
    store.clear_tmp().await;

    let tenancy = suhms_server::tenancy(Arc::clone(&store), &config);
    spawn_maintenance(&config, &store, &tenancy);

    if let Some(addr) = config.admin.listen {
        let token = config.admin.load_token()?;
        let admin = suhms_server::admin_api::router(
            Arc::clone(&store),
            Arc::clone(&tenancy),
            Arc::clone(&cipher),
            &token,
            config.s3.allow_public_buckets,
        );
        let listener = TcpListener::bind(addr).await?;
        tracing::info!(%addr, "admin API up");
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, admin)
                .with_graceful_shutdown(shutdown_signal())
                .await
            {
                tracing::error!(%error, "admin listener failed");
            }
        });
    }

    if config.cluster.is_enabled() {
        let addr = config
            .cluster
            .listen
            .ok_or_else(|| anyhow::anyhow!("[cluster] listen is required in cluster mode"))?;
        let secret = config.cluster.load_secret()?;
        let internal = suhms_server::cluster::internal_router(Arc::clone(&store), &secret);
        let listener = TcpListener::bind(addr).await?;
        match &config.cluster.tls {
            Some(tls) => {
                let tls_config = suhms_server::tls::server_config(tls)?;
                let listener = suhms_server::tls::TlsListener::new(listener, tls_config)?;
                tracing::info!(%addr, "cluster listener up (mutual TLS)");
                tokio::spawn(async move {
                    if let Err(error) = axum::serve(listener, internal)
                        .with_graceful_shutdown(shutdown_signal())
                        .await
                    {
                        tracing::error!(%error, "cluster listener failed");
                    }
                });
            }
            None => {
                tracing::warn!(%addr, "cluster listener up WITHOUT TLS; protect node-to-node traffic with a private network");
                tokio::spawn(async move {
                    if let Err(error) = axum::serve(listener, internal)
                        .with_graceful_shutdown(shutdown_signal())
                        .await
                    {
                        tracing::error!(%error, "cluster listener failed");
                    }
                });
            }
        }
    }

    let app = app(Arc::clone(&store), Arc::clone(&tenancy), cipher, &config)?;
    let listener = TcpListener::bind(config.server.listen).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    if let Err(error) = tenancy.meter.flush(&store).await {
        tracing::warn!(%error, "final usage flush failed");
    }
    tracing::info!("shut down cleanly");
    Ok(())
}

/// Runs `job` every `period` (first run after one period) in the background.
fn every<F, Fut>(period: std::time::Duration, name: &'static str, job: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send,
{
    if period.is_zero() {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(error) = job().await {
                tracing::warn!(job = name, %error, "background job failed");
            }
        }
    });
}

fn spawn_maintenance(
    config: &Config,
    store: &Arc<suhms_store::Store>,
    tenancy: &Arc<suhms_server::tenancy::Tenancy>,
) {
    use std::time::Duration;
    let m = &config.maintenance;
    let secs = Duration::from_secs;

    let s = Arc::clone(store);
    every(suhms_store::HEARTBEAT_INTERVAL, "heartbeat", move || {
        let s = Arc::clone(&s);
        async move { Ok(s.heartbeat().await?) }
    });
    let (s, t) = (Arc::clone(store), Arc::clone(tenancy));
    every(secs(m.usage_flush_secs), "usage flush", move || {
        let (s, t) = (Arc::clone(&s), Arc::clone(&t));
        async move { Ok(t.meter.flush(&s).await?) }
    });
    let s = Arc::clone(store);
    every(secs(m.scrub_interval_secs), "scrub", move || {
        let s = Arc::clone(&s);
        async move {
            let r = s.scrub().await?;
            tracing::info!(checked = r.checked, removed = r.removed, "scrub finished");
            Ok(())
        }
    });
    let (s, gc) = (
        Arc::clone(store),
        suhms_store::GcOptions {
            grace: secs(m.gc_grace_secs),
            force: false,
        },
    );
    every(secs(m.gc_interval_secs), "gc", move || {
        let s = Arc::clone(&s);
        async move {
            let r = s.gc(gc).await?;
            if r != suhms_store::GcReport::default() {
                tracing::info!(
                    orphans = r.orphans,
                    orphan_bytes = r.orphan_bytes,
                    uploads = r.uploads,
                    tmp = r.tmp,
                    purged = r.purged,
                    skipped_disks = r.skipped_disks,
                    "garbage collection finished"
                );
            }
            Ok(())
        }
    });
    // Every node tries; the lease lets one at a time run, so repair survives losing a node.
    for (period, full, name) in [
        (m.repair_interval_secs, false, "repair"),
        (m.full_repair_interval_secs, true, "full repair"),
    ] {
        let s = Arc::clone(store);
        every(secs(period), name, move || {
            let s = Arc::clone(&s);
            async move {
                let r = if full {
                    s.repair_full().await
                } else {
                    s.repair().await
                };
                match r {
                    Ok(r) => {
                        if r != suhms_store::RepairReport::default() {
                            tracing::info!(
                                job = name,
                                copied = r.copied,
                                trimmed = r.trimmed,
                                drained = r.drained,
                                queued = r.queued,
                                postponed = r.postponed,
                                resumed = r.resumed,
                                unrecoverable = r.unrecoverable,
                                "repair finished"
                            );
                        }
                        Ok(())
                    }
                    Err(suhms_store::StoreError::Busy(reason)) => {
                        tracing::debug!(job = name, %reason, "skipped");
                        Ok(())
                    }
                    Err(e) => Err(e.into()),
                }
            }
        });
    }
    if !m.leader {
        return;
    }
    let (s, max_age) = (Arc::clone(store), secs(m.abort_uploads_after_secs));
    every(secs(3600), "abort stale uploads", move || {
        let s = Arc::clone(&s);
        async move {
            let n = s.abort_stale_uploads(max_age).await?;
            if n > 0 {
                tracing::info!(aborted = n, "aborted stale multipart uploads");
            }
            Ok(())
        }
    });
    let (s, rate) = (Arc::clone(store), m.checksum_backfill_mib_per_sec);
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    every(
        secs(m.checksum_backfill_interval_secs),
        "checksum backfill",
        move || {
            let (s, finished) = (Arc::clone(&s), Arc::clone(&finished));
            async move {
                use std::sync::atomic::Ordering;
                if finished.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let options = suhms_server::backfill::BackfillOptions {
                    max_objects: None,
                    bytes_per_sec: (rate > 0).then_some(rate << 20),
                };
                match suhms_server::backfill::backfill_checksums(&s, options).await {
                    Ok(r) if r.is_idle() => {
                        // New uploads always get a checksum, so there is nothing more to do.
                        finished.store(true, Ordering::Relaxed);
                        Ok(())
                    }
                    Ok(r) => {
                        tracing::info!(
                            updated = r.updated,
                            corrupt = r.corrupt,
                            failed = r.failed,
                            "checksum backfill finished"
                        );
                        Ok(())
                    }
                    Err(suhms_store::StoreError::Busy(reason)) => {
                        tracing::debug!(%reason, "checksum backfill skipped");
                        Ok(())
                    }
                    Err(e) => Err(e.into()),
                }
            }
        },
    );
    let s = Arc::clone(store);
    every(secs(3600), "lifecycle", move || {
        let s = Arc::clone(&s);
        async move {
            let r = s.apply_lifecycle().await?;
            if r != suhms_store::LifecycleReport::default() {
                tracing::info!(
                    expired = r.expired,
                    purged = r.purged,
                    markers = r.markers,
                    transitioned = r.transitioned,
                    aborted = r.aborted,
                    "lifecycle applied"
                );
            }
            Ok(())
        }
    });
    let s = Arc::clone(store);
    every(
        secs(m.storage_snapshot_secs),
        "storage snapshot",
        move || {
            let s = Arc::clone(&s);
            async move {
                s.snapshot_storage().await?;
                Ok(())
            }
        },
    );
}

fn init_tracing(config: &Config) {
    // The audit trail stays on whatever the general level is.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("{},suhms::audit=info", config.log.level)));
    let builder = fmt().with_env_filter(filter).with_writer(std::io::stderr);
    match config.log.format {
        LogFormat::Pretty => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

/// Resolves on Ctrl+C or SIGTERM (systemd stop) so in-flight requests can finish.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
