//! Per-request tenant handling: who is calling (cached), whether they may (suspension, rate
//! limit), and what they used (the billing meter and Prometheus counters).

use std::{
    collections::HashMap,
    fmt::Write as _,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use suhms_store::{AccessKeyRecord, Store, TenantRecord, UsageDelta, hour_of};
use tokio::sync::RwLock;

const CACHE_TTL: Duration = Duration::from_secs(10);

/// Access keys and tenants, cached briefly so authentication does not hit the database on
/// every request. Disabling a key or suspending a tenant takes effect within [`CACHE_TTL`].
pub struct Directory {
    store: Arc<Store>,
    keys: RwLock<HashMap<String, (Instant, Option<AccessKeyRecord>)>>,
    tenants: RwLock<HashMap<String, (Instant, Option<TenantRecord>)>>,
}

impl Directory {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            keys: RwLock::default(),
            tenants: RwLock::default(),
        }
    }

    pub async fn key(&self, id: &str) -> suhms_store::Result<Option<AccessKeyRecord>> {
        if let Some((at, v)) = self.keys.read().await.get(id)
            && at.elapsed() < CACHE_TTL
        {
            return Ok(v.clone());
        }
        let v = self.store.metadata().get_access_key(id).await?;
        self.keys
            .write()
            .await
            .insert(id.to_owned(), (Instant::now(), v.clone()));
        Ok(v)
    }

    /// Forgets a cached key so a change made through this node applies at once.
    pub async fn invalidate_key(&self, id: &str) {
        self.keys.write().await.remove(id);
    }

    pub async fn invalidate_tenant(&self, id: &str) {
        self.tenants.write().await.remove(id);
    }

    pub async fn tenant(&self, id: &str) -> suhms_store::Result<Option<TenantRecord>> {
        if let Some((at, v)) = self.tenants.read().await.get(id)
            && at.elapsed() < CACHE_TTL
        {
            return Ok(v.clone());
        }
        let v = self.store.metadata().get_tenant(id).await?;
        self.tenants
            .write()
            .await
            .insert(id.to_owned(), (Instant::now(), v.clone()));
        Ok(v)
    }
}

/// Token buckets per access key.
pub struct RateLimiter {
    rate: f64,
    burst: f64,
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
}

impl RateLimiter {
    pub fn new(rate: f64, burst: f64) -> Self {
        Self {
            rate,
            burst: burst.max(1.0),
            buckets: Mutex::default(),
        }
    }

    /// Takes one token for `key`; false when the key is over its limit.
    pub fn allow(&self, key: &str) -> bool {
        if self.rate <= 0.0 {
            return true;
        }
        let now = Instant::now();
        let mut buckets = self.buckets.lock().expect("rate limiter lock");
        let (tokens, last) = buckets.entry(key.to_owned()).or_insert((self.burst, now));
        *tokens = (*tokens + now.duration_since(*last).as_secs_f64() * self.rate).min(self.burst);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// S3 pricing classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// PUT, COPY, POST, LIST.
    A,
    /// GET, HEAD and everything else.
    B,
}

/// In-memory usage counters, flushed periodically to the metadata store's hourly meter.
#[derive(Default)]
pub struct Meter {
    pending: Mutex<HashMap<(String, i64), UsageDelta>>,
    pub requests_total: AtomicU64,
    pub ingress_total: AtomicU64,
    pub egress_total: AtomicU64,
    pub rejected_total: AtomicU64,
}

fn now_hour() -> i64 {
    hour_of(crate::admin::now_millis())
}

impl Meter {
    fn with(&self, tenant: &str, f: impl FnOnce(&mut UsageDelta)) {
        let hour = now_hour();
        let mut pending = self.pending.lock().expect("meter lock");
        let d = pending
            .entry((tenant.to_owned(), hour))
            .or_insert_with(|| UsageDelta {
                tenant_id: tenant.to_owned(),
                hour,
                ..UsageDelta::default()
            });
        f(d);
    }

    pub fn request(&self, tenant: &str, class: Class) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.with(tenant, |d| match class {
            Class::A => d.class_a += 1,
            Class::B => d.class_b += 1,
        });
    }

    pub fn ingress(&self, tenant: &str, bytes: u64) {
        self.ingress_total.fetch_add(bytes, Ordering::Relaxed);
        self.with(tenant, |d| d.ingress_bytes += bytes);
    }

    pub fn egress(&self, tenant: &str, bytes: u64) {
        self.egress_total.fetch_add(bytes, Ordering::Relaxed);
        self.with(tenant, |d| d.egress_bytes += bytes);
    }

    /// Writes the pending counters to the meter. On failure they are kept for the next try.
    pub async fn flush(&self, store: &Store) -> suhms_store::Result<()> {
        let batch: Vec<UsageDelta> = {
            let mut pending = self.pending.lock().expect("meter lock");
            pending.drain().map(|(_, d)| d).collect()
        };
        if batch.is_empty() {
            return Ok(());
        }
        if let Err(e) = store.metadata().add_usage(&batch).await {
            let mut pending = self.pending.lock().expect("meter lock");
            for d in batch {
                let e = pending
                    .entry((d.tenant_id.clone(), d.hour))
                    .or_insert_with(|| UsageDelta {
                        tenant_id: d.tenant_id.clone(),
                        hour: d.hour,
                        ..UsageDelta::default()
                    });
                e.class_a += d.class_a;
                e.class_b += d.class_b;
                e.ingress_bytes += d.ingress_bytes;
                e.egress_bytes += d.egress_bytes;
            }
            return Err(e);
        }
        Ok(())
    }
}

/// Everything an S3 request needs besides the store.
pub struct Tenancy {
    pub directory: Directory,
    pub limiter: RateLimiter,
    pub meter: Meter,
    pub allow_public: bool,
}

impl Tenancy {
    pub fn new(
        store: Arc<Store>,
        limits: &crate::config::LimitsConfig,
        allow_public: bool,
    ) -> Self {
        Self {
            directory: Directory::new(store),
            limiter: RateLimiter::new(limits.requests_per_second, limits.burst),
            meter: Meter::default(),
            allow_public,
        }
    }

    /// Prometheus text exposition of this node's counters and the cluster's disks.
    pub async fn metrics(&self, store: &Store) -> String {
        let mut out = String::new();
        let m = &self.meter;
        for (name, help, v) in [
            (
                "suhms_s3_requests_total",
                "Authenticated S3 requests.",
                &m.requests_total,
            ),
            (
                "suhms_s3_rejected_total",
                "Requests refused (suspended, rate limited).",
                &m.rejected_total,
            ),
            (
                "suhms_s3_ingress_bytes_total",
                "Bytes uploaded.",
                &m.ingress_total,
            ),
            (
                "suhms_s3_egress_bytes_total",
                "Bytes downloaded.",
                &m.egress_total,
            ),
        ] {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {}",
                v.load(Ordering::Relaxed)
            );
        }
        if let Ok(disks) = store.cluster_disks().await {
            out.push_str("# HELP suhms_disk_alive Whether a disk is online and heartbeating.\n# TYPE suhms_disk_alive gauge\n");
            for d in disks.iter() {
                let _ = writeln!(
                    out,
                    "suhms_disk_alive{{node=\"{}\",disk=\"{}\"}} {}",
                    d.node_id,
                    d.id,
                    u8::from(d.alive)
                );
            }
            out.push_str("# HELP suhms_disk_free_bytes Free space reported for a disk.\n# TYPE suhms_disk_free_bytes gauge\n");
            for d in disks.iter() {
                let _ = writeln!(
                    out,
                    "suhms_disk_free_bytes{{node=\"{}\",disk=\"{}\"}} {}",
                    d.node_id, d.id, d.free
                );
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_allows_bursts_then_throttles() {
        let rl = RateLimiter::new(1.0, 3.0);
        assert!((0..3).all(|_| rl.allow("k")));
        assert!(!rl.allow("k"));
        assert!(rl.allow("other"), "limits are per key");
        assert!(RateLimiter::new(0.0, 0.0).allow("k"), "0 disables limiting");
    }

    #[test]
    fn meter_groups_by_tenant_and_hour() {
        let m = Meter::default();
        m.request("t", Class::A);
        m.request("t", Class::B);
        m.request("t", Class::B);
        m.ingress("t", 10);
        m.egress("u", 5);
        let pending = m.pending.lock().unwrap();
        let t = pending.values().find(|d| d.tenant_id == "t").unwrap();
        assert_eq!((t.class_a, t.class_b, t.ingress_bytes), (1, 2, 10));
        assert_eq!(pending.len(), 2);
    }
}
