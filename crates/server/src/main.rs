use std::{path::PathBuf, sync::Arc};

use clap::{Parser, Subcommand};
use suhms_server::{
    admin, app,
    config::{Config, LogFormat},
    crypto::SecretCipher,
};
use suhms_store::Store;
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
    let store = Arc::new(Store::open(config.storage.clone()).await?);
    store.clear_tmp().await;
    let app = app(store, cipher, &config)?;

    let listener = TcpListener::bind(config.server.listen).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("shut down cleanly");
    Ok(())
}

fn init_tracing(config: &Config) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log.level));
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
