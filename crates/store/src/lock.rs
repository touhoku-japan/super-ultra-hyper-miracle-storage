//! Object Lock (WORM) settings of a bucket. Objects carry their own retention and legal hold
//! (see `ObjectRecord::lock_mode`, `lock_until`, `legal_hold`).

use serde::{Deserialize, Serialize};

pub const GOVERNANCE: &str = "GOVERNANCE";
pub const COMPLIANCE: &str = "COMPLIANCE";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketLock {
    pub enabled: bool,
    /// Retention applied to new objects that do not specify their own.
    pub default_mode: Option<String>,
    pub default_days: Option<u32>,
}

pub fn encode(lock: &BucketLock) -> String {
    serde_json::to_string(lock).expect("lock config serializes")
}

pub fn decode(s: Option<&str>) -> BucketLock {
    s.and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}

pub fn valid_mode(mode: &str) -> bool {
    mode == GOVERNANCE || mode == COMPLIANCE
}
