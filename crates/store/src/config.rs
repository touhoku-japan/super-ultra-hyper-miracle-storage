use std::{collections::BTreeMap, path::PathBuf};

use serde::Deserialize;

use crate::error::{Result, StoreError};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StoreConfig {
    /// Identifies this node; replicas of one object are spread across distinct nodes.
    pub node_id: String,
    /// Mount points (or plain directories) holding object files. Each must already exist.
    pub disks: Vec<PathBuf>,
    /// Metadata backend URL, e.g. `sqlite:///var/lib/suhms/meta.db`.
    pub metadata_url: String,
    /// Replica count used when neither the object nor its bucket specifies one.
    pub default_replicas: u8,
    /// S3 storage class name -> replica count.
    pub storage_classes: BTreeMap<String, u8>,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            node_id: "local".to_owned(),
            disks: Vec::new(),
            metadata_url: "sqlite://suhms-meta.db".to_owned(),
            default_replicas: 3,
            storage_classes: BTreeMap::from([
                ("STANDARD".to_owned(), 3),
                ("STANDARD_IA".to_owned(), 2),
                ("REDUCED_REDUNDANCY".to_owned(), 1),
            ]),
        }
    }
}

impl StoreConfig {
    pub fn validate(&self) -> Result<()> {
        if self.node_id.is_empty() {
            return Err(StoreError::Config("node_id must not be empty".into()));
        }
        if self.disks.is_empty() {
            return Err(StoreError::Config("at least one disk is required".into()));
        }
        if self.default_replicas == 0 {
            return Err(StoreError::Config(
                "default_replicas must be at least 1".into(),
            ));
        }
        if let Some((name, _)) = self.storage_classes.iter().find(|(_, n)| **n == 0) {
            return Err(StoreError::Config(format!(
                "storage class {name} must have at least 1 replica"
            )));
        }
        Ok(())
    }
}
