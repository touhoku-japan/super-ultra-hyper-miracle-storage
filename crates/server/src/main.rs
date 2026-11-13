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
    /// Bring every object to its desired number of copies
    Repair,
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
        Command::Repair => admin::repair(config).await,
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
    if !m.leader {
        return;
    }
    let s = Arc::clone(store);
    every(secs(m.repair_interval_secs), "repair", move || {
        let s = Arc::clone(&s);
        async move {
            let r = s.repair().await?;
            if r.copied + r.trimmed + r.unrecoverable > 0 {
                tracing::info!(
                    copied = r.copied,
                    trimmed = r.trimmed,
                    unrecoverable = r.unrecoverable,
                    "repair finished"
                );
            }
            Ok(())
        }
    });
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
