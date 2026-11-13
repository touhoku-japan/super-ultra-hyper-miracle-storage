use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::Deserialize;
use suhms_store::StoreConfig;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub log: LogConfig,
    pub storage: StoreConfig,
    pub s3: S3Config,
    pub auth: AuthConfig,
    pub cluster: ClusterConfig,
    pub admin: AdminConfig,
    pub limits: LimitsConfig,
    pub maintenance: MaintenanceConfig,
}

/// The administration API (used by the SoraHost portal). Disabled unless `listen` is set.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminConfig {
    /// Keep it on a private network.
    pub listen: Option<SocketAddr>,
    /// File holding the bearer token; `SUHMS_ADMIN_TOKEN` takes precedence.
    pub token_file: Option<PathBuf>,
}

impl AdminConfig {
    pub fn load_token(&self) -> anyhow::Result<String> {
        let token = match std::env::var("SUHMS_ADMIN_TOKEN") {
            Ok(t) => t,
            Err(_) => {
                let path = self
                    .token_file
                    .as_ref()
                    .context("the admin API needs SUHMS_ADMIN_TOKEN or [admin] token_file")?;
                std::fs::read_to_string(path)
                    .with_context(|| format!("failed to read {}", path.display()))?
            }
        };
        let token = token.trim().to_owned();
        anyhow::ensure!(
            token.len() >= 32,
            "admin token must be at least 32 characters"
        );
        Ok(token)
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// Sustained requests per second allowed per access key (on each node).
    pub requests_per_second: f64,
    /// Short bursts above the sustained rate.
    pub burst: f64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            requests_per_second: 200.0,
            burst: 400.0,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaintenanceConfig {
    /// Run cluster-wide jobs (repair, upload cleanup, usage snapshots) on this node. Enable it
    /// on exactly one node of a cluster.
    pub leader: bool,
    pub repair_interval_secs: u64,
    /// Every node scrubs its own disks; 0 disables.
    pub scrub_interval_secs: u64,
    /// Multipart uploads older than this are aborted.
    pub abort_uploads_after_secs: u64,
    /// How often request/traffic counters are written to the usage meter.
    pub usage_flush_secs: u64,
    /// How often stored bytes are recorded in the usage meter.
    pub storage_snapshot_secs: u64,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            leader: true,
            repair_interval_secs: 600,
            scrub_interval_secs: 7 * 24 * 3600,
            abort_uploads_after_secs: 7 * 24 * 3600,
            usage_flush_secs: 60,
            storage_snapshot_secs: 3600,
        }
    }
}

/// Multi-node operation. Leave `nodes` empty for a single-node deployment.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClusterConfig {
    /// Address for node-to-node traffic. Keep it on a private network.
    pub listen: Option<SocketAddr>,
    /// File holding the shared secret that authenticates node-to-node requests. The
    /// `SUHMS_CLUSTER_SECRET` environment variable takes precedence.
    pub secret_file: Option<PathBuf>,
    /// Every node of the cluster (including this one), by `[storage] node_id`.
    pub nodes: Vec<NodeConfig>,
    /// Mutual TLS between nodes. Without it node-to-node traffic is plain HTTP and must be
    /// protected by the network (private network or WireGuard).
    pub tls: Option<ClusterTls>,
}

/// Certificates for node-to-node TLS (PEM files).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterTls {
    /// This node's certificate (chain), signed by the cluster CA. It must name the host or IP
    /// that other nodes use in its `url`.
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    /// The cluster CA. Only certificates it signed are accepted, on both sides.
    pub ca_file: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub id: String,
    /// Base URL of the node's cluster listener, e.g. `https://10.0.0.2:9100` (`http://`
    /// without `[cluster.tls]`).
    pub url: String,
}

impl ClusterConfig {
    pub fn is_enabled(&self) -> bool {
        !self.nodes.is_empty()
    }

    /// Node URLs must all use `https://` with `[cluster.tls]` and `http://` without it, so a
    /// half-configured cluster fails at startup instead of on the first request.
    pub fn validate(&self) -> anyhow::Result<()> {
        let scheme = if self.tls.is_some() {
            "https://"
        } else {
            "http://"
        };
        for n in &self.nodes {
            anyhow::ensure!(
                n.url.starts_with(scheme),
                "[cluster] node {} url {} must start with {scheme} ({})",
                n.id,
                n.url,
                if self.tls.is_some() {
                    "[cluster.tls] is configured"
                } else {
                    "[cluster.tls] is not configured"
                }
            );
        }
        Ok(())
    }

    pub fn load_secret(&self) -> anyhow::Result<String> {
        if let Ok(s) = std::env::var("SUHMS_CLUSTER_SECRET") {
            return Ok(s.trim().to_owned());
        }
        let path = self
            .secret_file
            .as_ref()
            .context("cluster mode needs SUHMS_CLUSTER_SECRET or [cluster] secret_file")?;
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let s = s.trim().to_owned();
        anyhow::ensure!(
            s.len() >= 32,
            "cluster secret must be at least 32 characters"
        );
        Ok(s)
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct S3Config {
    /// Region name clients sign requests for (and that GetBucketLocation reports).
    pub region: String,
    /// Base domains for virtual-hosted-style requests (`<bucket>.<domain>`). Path-style
    /// requests always work.
    pub domains: Vec<String>,
    /// Largest single PutObject body, in bytes.
    pub max_object_size: u64,
    /// Allow buckets to be made public-read (anonymous GET/HEAD) through the admin API.
    pub allow_public_buckets: bool,
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            region: "jp-1".to_owned(),
            domains: Vec::new(),
            max_object_size: 5 * 1024 * 1024 * 1024,
            allow_public_buckets: false,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// File holding the hex master key that encrypts access key secrets. The
    /// `SUHMS_MASTER_KEY` environment variable takes precedence.
    pub master_key_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: SocketAddr,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 9000)),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Pretty,
    Json,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    pub format: LogFormat,
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::Pretty,
            level: "info".to_owned(),
        }
    }
}

impl Config {
    /// Loads the config from `path`, or returns defaults when no path is given.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        toml::from_str(&text)
            .with_context(|| format!("failed to parse config file {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../config.example.toml");
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.server.listen.port(), 9000);
        assert_eq!(cfg.log.format, LogFormat::Pretty);
        assert_eq!(cfg.s3.region, "jp-1");
        assert_eq!(cfg.storage.disks.len(), 3);
        assert_eq!(cfg.storage.storage_classes.get("STANDARD"), Some(&3));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>("[server]\nlisten_typo = \"x\"").is_err());
    }
}
