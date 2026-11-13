//! Bucket lifecycle rules (a subset of S3's): expire current versions, permanently delete
//! noncurrent versions, remove expired object delete markers, move versions to another
//! storage class (= replica count) and abort incomplete multipart uploads, each after a
//! number of days, optionally limited to a key prefix.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleRule {
    pub id: String,
    #[serde(default)]
    pub prefix: String,
    pub enabled: bool,
    /// Current versions older than this are deleted (or get a delete marker when versioned).
    pub expiration_days: Option<u32>,
    /// Versions that have been noncurrent for this long are permanently deleted.
    pub noncurrent_days: Option<u32>,
    /// Multipart uploads started this long ago are aborted.
    pub abort_multipart_days: Option<u32>,
    /// Delete markers that are the only remaining version of their key are removed.
    #[serde(default)]
    pub expired_object_delete_marker: bool,
    /// Current versions this old move to another storage class.
    #[serde(default)]
    pub transitions: Vec<Transition>,
    /// Versions noncurrent for this long move to another storage class.
    #[serde(default)]
    pub noncurrent_transitions: Vec<Transition>,
}

/// After `days`, a version's storage class (and so its replica count) becomes
/// `storage_class`; repair then adds or removes copies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub days: u32,
    pub storage_class: String,
}

pub fn encode(rules: &[LifecycleRule]) -> String {
    serde_json::to_string(rules).expect("rules serialize")
}

pub fn decode(s: &str) -> Vec<LifecycleRule> {
    serde_json::from_str(s).unwrap_or_default()
}
