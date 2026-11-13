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
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            region: "jp-1".to_owned(),
            domains: Vec::new(),
            max_object_size: 5 * 1024 * 1024 * 1024,
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
