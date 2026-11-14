//! Bucket lifecycle rules (a subset of S3's): expire current versions, permanently delete
//! noncurrent versions, remove expired object delete markers, move versions to another
//! storage class (= replica count) and abort incomplete multipart uploads, each after a
//! number of days or from a date, optionally limited to a key prefix, an object size range and
//! object tags.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::meta::ObjectRecord;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleRule {
    pub id: String,
    #[serde(default)]
    pub prefix: String,
    pub enabled: bool,
    /// Current versions older than this are deleted (or get a delete marker when versioned).
    pub expiration_days: Option<u32>,
    /// From this time (unix ms) on, every current version is expired.
    #[serde(default)]
    pub expiration_date: Option<i64>,
    /// Versions that have been noncurrent for this long are permanently deleted.
    pub noncurrent_days: Option<u32>,
    /// Keeps this many of the newest noncurrent versions of each key from
    /// `noncurrent_days` expiration.
    #[serde(default)]
    pub newer_noncurrent_versions: Option<u32>,
    /// Only objects larger than this many bytes (`ObjectSizeGreaterThan`).
    #[serde(default)]
    pub min_size: Option<u64>,
    /// Only objects smaller than this many bytes (`ObjectSizeLessThan`).
    #[serde(default)]
    pub max_size: Option<u64>,
    /// Only objects having all of these tags.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
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

/// After `days` (or from `date`), a version's storage class (and so its replica count)
/// becomes `storage_class`; repair then adds or removes copies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    #[serde(default)]
    pub days: u32,
    pub storage_class: String,
    /// Unix ms; when set, `days` is ignored and every version qualifies from then on.
    #[serde(default)]
    pub date: Option<i64>,
    /// Noncurrent transitions only: keep this many of the newest noncurrent versions.
    #[serde(default)]
    pub newer_noncurrent_versions: Option<u32>,
}

impl LifecycleRule {
    /// Whether a version falls under the rule's size and tag filters. Delete markers have no
    /// size or tags and are never filtered out (S3 does not allow tag filters together with
    /// expired-delete-marker removal).
    pub fn matches(&self, o: &ObjectRecord) -> bool {
        o.delete_marker
            || (self.min_size.is_none_or(|min| o.size > min)
                && self.max_size.is_none_or(|max| o.size < max)
                && self.tags.iter().all(|(k, v)| o.tags.get(k) == Some(v)))
    }
}

/// Cutoff (unix ms) for an action after `days` or from `date`: versions whose age starts
/// before it qualify. `None` while a date has not been reached.
pub fn cutoff(now: i64, days: Option<u32>, date: Option<i64>) -> Option<i64> {
    match date {
        Some(d) => (now >= d).then_some(i64::MAX),
        None => days.map(|d| now - i64::from(d) * 86_400_000),
    }
}

pub fn encode(rules: &[LifecycleRule]) -> String {
    serde_json::to_string(rules).expect("rules serialize")
}

pub fn decode(s: &str) -> Vec<LifecycleRule> {
    serde_json::from_str(s).unwrap_or_default()
}
