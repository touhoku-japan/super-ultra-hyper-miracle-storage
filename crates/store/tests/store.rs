use std::{collections::HashSet, path::PathBuf};

use md5::{Digest, Md5};
use suhms_store::{PutObjectRequest, Store, StoreConfig, StoreError};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;

struct Env {
    dir: TempDir,
    disks: Vec<PathBuf>,
}

impl Env {
    fn new(disks: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let disks = (0..disks)
            .map(|i| {
                let p = dir.path().join(format!("disk{i}"));
                std::fs::create_dir(&p).unwrap();
                p
            })
            .collect();
        Self { dir, disks }
    }

    fn config(&self) -> StoreConfig {
        StoreConfig {
            disks: self.disks.clone(),
            metadata_url: format!("sqlite://{}", self.dir.path().join("meta.db").display()),
            ..StoreConfig::default()
        }
    }

    async fn open(&self) -> Store {
        Store::open(self.config()).await.unwrap()
    }
}

/// 3 MiB of non-repeating-ish bytes, so chunking and hashing see real data.
fn payload() -> Vec<u8> {
    (0..3 * 1024 * 1024u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

fn put(bucket: &str, key: &str, class: Option<&str>) -> PutObjectRequest {
    PutObjectRequest {
        bucket: bucket.into(),
        key: key.into(),
        content_type: Some("application/octet-stream".into()),
        storage_class: class.map(Into::into),
    }
}

async fn read_object(store: &Store, bucket: &str, key: &str) -> Vec<u8> {
    let (_, mut file) = store.get_object(bucket, key).await.unwrap();
    let mut out = Vec::new();
    file.read_to_end(&mut out).await.unwrap();
    out
}

fn disk_of(path: &std::path::Path, env: &Env) -> usize {
    env.disks.iter().position(|d| path.starts_with(d)).unwrap()
}

#[tokio::test]
async fn put_writes_plain_identical_files_on_distinct_disks() {
    let env = Env::new(4);
    let store = env.open().await;
    store.create_bucket("backups", None).await.unwrap();
    let data = payload();

    let rec = store
        .put_object(put("backups", "dir/a.bin", None), &data[..])
        .await
        .unwrap();
    assert_eq!(rec.size, data.len() as u64);
    assert_eq!(rec.etag, hex::encode(Md5::digest(&data)));
    assert_eq!(rec.desired_replicas, 3);

    let locs = store
        .object_locations("backups", "dir/a.bin")
        .await
        .unwrap();
    assert_eq!(locs.len(), 3);
    let disks: HashSet<_> = locs.iter().map(|p| disk_of(p, &env)).collect();
    assert_eq!(disks.len(), 3, "copies must be on distinct disks");
    for p in &locs {
        assert_eq!(std::fs::read(p).unwrap(), data, "file is stored verbatim");
    }
    assert_eq!(read_object(&store, "backups", "dir/a.bin").await, data);
}

#[tokio::test]
async fn replica_count_follows_class_then_bucket_then_default() {
    let env = Env::new(4);
    let store = env.open().await;
    store.create_bucket("one", Some(1)).await.unwrap();
    store.create_bucket("dflt", None).await.unwrap();

    let count = |b: &'static str, k: &'static str| {
        let store = &store;
        async move { store.object_locations(b, k).await.unwrap().len() }
    };

    store
        .put_object(put("one", "a", None), &b"x"[..])
        .await
        .unwrap();
    assert_eq!(count("one", "a").await, 1);
    store
        .put_object(put("one", "b", Some("STANDARD_IA")), &b"x"[..])
        .await
        .unwrap();
    assert_eq!(count("one", "b").await, 2);
    store
        .put_object(put("dflt", "c", None), &b"x"[..])
        .await
        .unwrap();
    assert_eq!(count("dflt", "c").await, 3);

    let err = store
        .put_object(put("one", "d", Some("GLACIER")), &b"x"[..])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownStorageClass(_)), "{err}");
    let err = store.create_bucket("five", Some(5)).await.unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::InvalidReplicaCount {
                requested: 5,
                max: 4
            }
        ),
        "{err}"
    );
}

#[tokio::test]
async fn overwrite_and_delete_remove_old_files() {
    let env = Env::new(3);
    let store = env.open().await;
    store.create_bucket("bk1", None).await.unwrap();

    store
        .put_object(put("bk1", "k", None), &b"old"[..])
        .await
        .unwrap();
    let old = store.object_locations("bk1", "k").await.unwrap();
    store
        .put_object(put("bk1", "k", None), &b"new"[..])
        .await
        .unwrap();
    assert!(
        old.iter().all(|p| !p.exists()),
        "old version's files are removed"
    );
    assert_eq!(read_object(&store, "bk1", "k").await, b"new");

    let err = store.delete_bucket("bk1").await.unwrap_err();
    assert!(matches!(err, StoreError::BucketNotEmpty(_)), "{err}");

    let cur = store.object_locations("bk1", "k").await.unwrap();
    assert!(store.delete_object("bk1", "k").await.unwrap());
    assert!(cur.iter().all(|p| !p.exists()));
    assert!(!store.delete_object("bk1", "k").await.unwrap());
    let err = store.get_object("bk1", "k").await.unwrap_err();
    assert!(matches!(err, StoreError::NoSuchKey { .. }), "{err}");

    store.delete_bucket("bk1").await.unwrap();
    let err = store.get_object("bk1", "k").await.unwrap_err();
    assert!(matches!(err, StoreError::NoSuchBucket(_)), "{err}");
}

#[tokio::test]
async fn get_falls_back_to_another_copy() {
    let env = Env::new(3);
    let store = env.open().await;
    store.create_bucket("bk1", None).await.unwrap();
    store
        .put_object(put("bk1", "k", None), &b"hello"[..])
        .await
        .unwrap();
    let locs = store.object_locations("bk1", "k").await.unwrap();
    std::fs::remove_file(&locs[0]).unwrap();
    std::fs::write(&locs[1], b"truncated?").unwrap(); // wrong size
    assert_eq!(read_object(&store, "bk1", "k").await, b"hello");
}

#[tokio::test]
async fn scrub_and_repair_restore_corrupt_and_missing_copies() {
    let env = Env::new(4);
    let store = env.open().await;
    store.create_bucket("bk1", None).await.unwrap();
    let data = payload();
    store
        .put_object(put("bk1", "k", None), &data[..])
        .await
        .unwrap();
    let locs = store.object_locations("bk1", "k").await.unwrap();

    // Same size, flipped content: only the hash can catch this.
    let mut bad = data.clone();
    bad[12345] ^= 0xff;
    std::fs::write(&locs[0], &bad).unwrap();
    std::fs::remove_file(&locs[1]).unwrap();

    let scrub = store.scrub().await.unwrap();
    assert_eq!(scrub.checked, 3);
    assert_eq!(scrub.removed, 2);
    assert_eq!(store.object_locations("bk1", "k").await.unwrap().len(), 1);

    let repair = store.repair().await.unwrap();
    assert_eq!(repair.copied, 2);
    assert_eq!(repair.unrecoverable, 0);

    let locs = store.object_locations("bk1", "k").await.unwrap();
    assert_eq!(locs.len(), 3);
    let disks: HashSet<_> = locs.iter().map(|p| disk_of(p, &env)).collect();
    assert_eq!(disks.len(), 3);
    for p in &locs {
        assert_eq!(std::fs::read(p).unwrap(), data);
    }
    assert_eq!(
        store.repair().await.unwrap(),
        Default::default(),
        "idempotent"
    );
}

#[tokio::test]
async fn changing_bucket_replicas_adds_or_trims_copies() {
    let env = Env::new(3);
    let store = env.open().await;
    store.create_bucket("bk1", None).await.unwrap();
    store
        .put_object(put("bk1", "auto", None), &b"a"[..])
        .await
        .unwrap();
    store
        .put_object(put("bk1", "pinned", Some("STANDARD")), &b"p"[..])
        .await
        .unwrap();

    store.set_bucket_replicas("bk1", Some(1)).await.unwrap();
    let r = store.repair().await.unwrap();
    assert_eq!(r.trimmed, 2);
    assert_eq!(
        store.object_locations("bk1", "auto").await.unwrap().len(),
        1
    );
    assert_eq!(
        store.object_locations("bk1", "pinned").await.unwrap().len(),
        3,
        "explicit storage class is not affected by the bucket setting"
    );
    assert_eq!(read_object(&store, "bk1", "auto").await, b"a");

    store.set_bucket_replicas("bk1", Some(2)).await.unwrap();
    let r = store.repair().await.unwrap();
    assert_eq!(r.copied, 1);
    assert_eq!(
        store.object_locations("bk1", "auto").await.unwrap().len(),
        2
    );
}

#[tokio::test]
async fn offline_disk_is_skipped_and_never_created() {
    let env = Env::new(3);
    {
        let store = env.open().await;
        store.create_bucket("bk1", None).await.unwrap();
        store
            .put_object(put("bk1", "k", None), &b"data"[..])
            .await
            .unwrap();
    }

    // Simulate an unmounted disk: its directory disappears.
    let gone = env.disks[2].clone();
    let moved = env.dir.path().join("unmounted");
    std::fs::rename(&gone, &moved).unwrap();

    let store = env.open().await;
    assert!(!gone.exists(), "a missing disk root must not be recreated");
    assert_eq!(read_object(&store, "bk1", "k").await, b"data");
    assert_eq!(store.max_replicas(), 2);
    // With one disk offline only 2 online copies remain and there is nowhere to add a third.
    let r = store.repair().await.unwrap();
    assert_eq!(r.copied, 0);

    // Disk comes back: identity is recognised, all 3 copies are online again.
    drop(store);
    std::fs::rename(&moved, &gone).unwrap();
    let store = env.open().await;
    assert_eq!(store.object_locations("bk1", "k").await.unwrap().len(), 3);
    assert_eq!(store.repair().await.unwrap(), Default::default());
}

#[tokio::test]
async fn refuses_to_format_a_non_empty_directory() {
    let env = Env::new(2);
    std::fs::write(env.disks[1].join("someone-elses-file"), b"x").unwrap();
    let store = env.open().await;
    assert_eq!(store.max_replicas(), 1, "the foreign directory is not used");
    assert!(!env.disks[1].join(".suhms").exists());
}

#[tokio::test]
async fn rejects_invalid_names() {
    let env = Env::new(1);
    let store = env.open().await;
    for bad in ["ab", "UPPER", "-start", "end-", "a..b", "has_underscore"] {
        assert!(
            matches!(
                store.create_bucket(bad, None).await,
                Err(StoreError::InvalidBucketName(_))
            ),
            "{bad}"
        );
    }
    store.create_bucket("ok-bucket.1", Some(1)).await.unwrap();
    let err = store
        .put_object(put("ok-bucket.1", "", None), &b""[..])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidObjectKey), "{err}");
}
