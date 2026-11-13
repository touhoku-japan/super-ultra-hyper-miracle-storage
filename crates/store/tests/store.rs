use std::{collections::HashSet, path::PathBuf};

mod common;

use md5::{Digest, Md5};
use suhms_store::{
    ListParams, ListStart, PutObjectRequest, Store, StoreConfig, StoreError, TenantRecord,
};

const OWNER: &str = "tenant-1";
use tempfile::TempDir;
use tokio::io::AsyncReadExt;

struct Env {
    dir: TempDir,
    disks: Vec<PathBuf>,
    meta_url: String,
}

impl Env {
    async fn new(disks: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let disks = (0..disks)
            .map(|i| {
                let p = dir.path().join(format!("disk{i}"));
                std::fs::create_dir(&p).unwrap();
                p
            })
            .collect();
        let meta_url = common::metadata_url(dir.path()).await;
        Self {
            dir,
            disks,
            meta_url,
        }
    }

    fn config(&self) -> StoreConfig {
        StoreConfig {
            disks: self.disks.clone(),
            metadata_url: self.meta_url.clone(),
            ..StoreConfig::default()
        }
    }

    async fn open(&self) -> Store {
        let store = Store::open(self.config()).await.unwrap();
        // Tests act as one tenant with a fixed id; creating it again on reopen is a no-op.
        store
            .metadata()
            .create_tenant(&TenantRecord {
                id: OWNER.into(),
                name: OWNER.into(),
                ..TenantRecord::default()
            })
            .await
            .unwrap();
        store
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
        ..Default::default()
    }
}

async fn read_object(store: &Store, bucket: &str, key: &str) -> Vec<u8> {
    let stored = store.get_object(OWNER, bucket, key).await.unwrap();
    let mut reader = store
        .read_object(&stored, 0..stored.object.size)
        .await
        .unwrap();
    let mut out = Vec::new();
    reader.read_to_end(&mut out).await.unwrap();
    out
}

fn disk_of(path: &std::path::Path, env: &Env) -> usize {
    env.disks.iter().position(|d| path.starts_with(d)).unwrap()
}

#[tokio::test]
async fn put_writes_plain_identical_files_on_distinct_disks() {
    let env = Env::new(4).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "backups", None).await.unwrap();
    let data = payload();

    let rec = store
        .put_object(OWNER, put("backups", "dir/a.bin", None), &data[..])
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
    let env = Env::new(4).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "one", Some(1)).await.unwrap();
    store.create_bucket(OWNER, "dflt", None).await.unwrap();

    let count = |b: &'static str, k: &'static str| {
        let store = &store;
        async move { store.object_locations(b, k).await.unwrap().len() }
    };

    store
        .put_object(OWNER, put("one", "a", None), &b"x"[..])
        .await
        .unwrap();
    assert_eq!(count("one", "a").await, 1);
    store
        .put_object(OWNER, put("one", "b", Some("STANDARD_IA")), &b"x"[..])
        .await
        .unwrap();
    assert_eq!(count("one", "b").await, 2);
    store
        .put_object(OWNER, put("dflt", "c", None), &b"x"[..])
        .await
        .unwrap();
    assert_eq!(count("dflt", "c").await, 3);

    let err = store
        .put_object(OWNER, put("one", "d", Some("GLACIER")), &b"x"[..])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownStorageClass(_)), "{err}");
    let err = store
        .create_bucket(OWNER, "five", Some(5))
        .await
        .unwrap_err();
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
    let env = Env::new(3).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "bk1", None).await.unwrap();

    store
        .put_object(OWNER, put("bk1", "k", None), &b"old"[..])
        .await
        .unwrap();
    let old = store.object_locations("bk1", "k").await.unwrap();
    store
        .put_object(OWNER, put("bk1", "k", None), &b"new"[..])
        .await
        .unwrap();
    assert!(
        old.iter().all(|p| !p.exists()),
        "old version's files are removed"
    );
    assert_eq!(read_object(&store, "bk1", "k").await, b"new");

    let err = store.delete_bucket(OWNER, "bk1").await.unwrap_err();
    assert!(matches!(err, StoreError::BucketNotEmpty(_)), "{err}");

    let cur = store.object_locations("bk1", "k").await.unwrap();
    store.delete_object(OWNER, "bk1", "k").await.unwrap();
    assert!(cur.iter().all(|p| !p.exists()));
    store.delete_object(OWNER, "bk1", "k").await.unwrap();
    let err = store.get_object(OWNER, "bk1", "k").await.unwrap_err();
    assert!(matches!(err, StoreError::NoSuchKey { .. }), "{err}");

    store.delete_bucket(OWNER, "bk1").await.unwrap();
    let err = store.get_object(OWNER, "bk1", "k").await.unwrap_err();
    assert!(matches!(err, StoreError::NoSuchBucket(_)), "{err}");
}

#[tokio::test]
async fn get_falls_back_to_another_copy() {
    let env = Env::new(3).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "bk1", None).await.unwrap();
    store
        .put_object(OWNER, put("bk1", "k", None), &b"hello"[..])
        .await
        .unwrap();
    let locs = store.object_locations("bk1", "k").await.unwrap();
    std::fs::remove_file(&locs[0]).unwrap();
    std::fs::write(&locs[1], b"truncated?").unwrap(); // wrong size
    assert_eq!(read_object(&store, "bk1", "k").await, b"hello");
}

#[tokio::test]
async fn scrub_and_repair_restore_corrupt_and_missing_copies() {
    let env = Env::new(4).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "bk1", None).await.unwrap();
    let data = payload();
    store
        .put_object(OWNER, put("bk1", "k", None), &data[..])
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
    let env = Env::new(3).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "bk1", None).await.unwrap();
    store
        .put_object(OWNER, put("bk1", "auto", None), &b"a"[..])
        .await
        .unwrap();
    store
        .put_object(OWNER, put("bk1", "pinned", Some("STANDARD")), &b"p"[..])
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
    let env = Env::new(3).await;
    {
        let store = env.open().await;
        store.create_bucket(OWNER, "bk1", None).await.unwrap();
        store
            .put_object(OWNER, put("bk1", "k", None), &b"data"[..])
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
    // Capacity still counts the registered (offline) disk, so 3-replica writes keep working
    // with 2 copies until it returns.
    assert_eq!(store.max_replicas().await.unwrap(), 3);
    store
        .put_object(OWNER, put("bk1", "while-degraded", None), &b"d"[..])
        .await
        .unwrap();
    assert_eq!(
        store
            .object_locations("bk1", "while-degraded")
            .await
            .unwrap()
            .len(),
        2
    );
    // With one disk offline only 2 online copies remain and there is nowhere to add a third.
    let r = store.repair().await.unwrap();
    assert_eq!(r.copied, 0);

    // Disk comes back: identity is recognised, all 3 copies are online again.
    drop(store);
    std::fs::rename(&moved, &gone).unwrap();
    let store = env.open().await;
    assert_eq!(store.object_locations("bk1", "k").await.unwrap().len(), 3);
    // The object written while degraded gets its third copy.
    let r = store.repair().await.unwrap();
    assert_eq!((r.copied, r.trimmed, r.unrecoverable), (1, 0, 0));
    assert_eq!(
        store
            .object_locations("bk1", "while-degraded")
            .await
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn refuses_to_format_a_non_empty_directory() {
    let env = Env::new(2).await;
    std::fs::write(env.disks[1].join("someone-elses-file"), b"x").unwrap();
    let store = env.open().await;
    assert_eq!(
        store.max_replicas().await.unwrap(),
        1,
        "the foreign directory is not used"
    );
    assert!(!env.disks[1].join(".suhms").exists());
}

#[tokio::test]
async fn rejects_invalid_names() {
    let env = Env::new(1).await;
    let store = env.open().await;
    for bad in ["ab", "UPPER", "-start", "end-", "a..b", "has_underscore"] {
        assert!(
            matches!(
                store.create_bucket(OWNER, bad, None).await,
                Err(StoreError::InvalidBucketName(_))
            ),
            "{bad}"
        );
    }
    store
        .create_bucket(OWNER, "ok-bucket.1", Some(1))
        .await
        .unwrap();
    let err = store
        .put_object(OWNER, put("ok-bucket.1", "", None), &b""[..])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidObjectKey), "{err}");
}

#[tokio::test]
async fn tenants_cannot_touch_each_others_buckets() {
    let env = Env::new(3).await;
    let store = env.open().await;
    let other = store.create_tenant("other-customer").await.unwrap();
    let err = store.create_tenant("other-customer").await.unwrap_err();
    assert!(matches!(err, StoreError::TenantAlreadyExists(_)), "{err}");

    store.create_bucket(OWNER, "mine", None).await.unwrap();
    store
        .put_object(OWNER, put("mine", "secret", None), &b"s"[..])
        .await
        .unwrap();

    let denied = |r: Result<_, StoreError>| matches!(r, Err(StoreError::AccessDenied));
    assert!(denied(
        store
            .get_object(&other.id, "mine", "secret")
            .await
            .map(|_| ())
    ));
    assert!(denied(
        store
            .head_object(&other.id, "mine", "secret")
            .await
            .map(|_| ())
    ));
    assert!(denied(
        store
            .put_object(&other.id, put("mine", "x", None), &b"x"[..])
            .await
            .map(|_| ())
    ));
    assert!(denied(
        store
            .delete_object(&other.id, "mine", "secret")
            .await
            .map(|_| ())
    ));
    assert!(denied(store.delete_bucket(&other.id, "mine").await));
    let params = ListParams {
        bucket: "mine".into(),
        prefix: String::new(),
        delimiter: None,
        start: ListStart::Beginning,
        max_keys: 1000,
    };
    assert!(denied(
        store.list_objects(&other.id, &params).await.map(|_| ())
    ));

    let err = store
        .create_bucket(&other.id, "mine", None)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::BucketAlreadyExists(_)), "{err}");
    let err = store.create_bucket(OWNER, "mine", None).await.unwrap_err();
    assert!(
        matches!(err, StoreError::BucketAlreadyOwnedByYou(_)),
        "{err}"
    );

    assert_eq!(store.list_buckets(&other.id).await.unwrap(), vec![]);
    assert_eq!(store.list_buckets(OWNER).await.unwrap().len(), 1);
    // Still intact for the owner.
    assert_eq!(read_object(&store, "mine", "secret").await, b"s");
}

#[tokio::test]
async fn content_md5_mismatch_is_rejected_and_leaves_nothing() {
    let env = Env::new(3).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "bk1", None).await.unwrap();
    store
        .put_object(OWNER, put("bk1", "k", None), &b"keep"[..])
        .await
        .unwrap();
    let before = store.object_locations("bk1", "k").await.unwrap();

    let mut req = put("bk1", "k", None);
    req.content_md5 = Some([0; 16]);
    let err = store
        .put_object(OWNER, req, &b"other"[..])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::BadDigest), "{err}");
    assert_eq!(
        read_object(&store, "bk1", "k").await,
        b"keep",
        "old version survives"
    );
    assert_eq!(store.object_locations("bk1", "k").await.unwrap(), before);

    let mut req = put("bk1", "k2", None);
    req.content_md5 = Some(Md5::digest(b"good").into());
    req.user_metadata.insert("mtime".into(), "123".into());
    let rec = store.put_object(OWNER, req, &b"good"[..]).await.unwrap();
    assert_eq!(
        rec.user_metadata.get("mtime").map(String::as_str),
        Some("123")
    );
    let head = store.head_object(OWNER, "bk1", "k2").await.unwrap();
    assert_eq!(head.user_metadata, rec.user_metadata);
}

async fn list_all(
    store: &Store,
    prefix: &str,
    delimiter: Option<&str>,
    page_size: usize,
    use_cursor: bool,
) -> (Vec<String>, Vec<String>, usize) {
    let mut keys = Vec::new();
    let mut prefixes = Vec::new();
    let mut pages = 0;
    let mut start = ListStart::Beginning;
    loop {
        let page = store
            .list_objects(
                OWNER,
                &ListParams {
                    bucket: "lst".into(),
                    prefix: prefix.into(),
                    delimiter: delimiter.map(Into::into),
                    start: start.clone(),
                    max_keys: page_size,
                },
            )
            .await
            .unwrap();
        pages += 1;
        keys.extend(page.objects.iter().map(|o| o.key.clone()));
        prefixes.extend(page.common_prefixes.iter().cloned());
        if !page.truncated {
            break;
        }
        start = if use_cursor {
            ListStart::Cursor(page.next_cursor.unwrap())
        } else {
            ListStart::Marker(page.next_marker.unwrap())
        };
    }
    (keys, prefixes, pages)
}

#[tokio::test]
async fn list_objects_follows_s3_semantics() {
    let env = Env::new(1).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "lst", Some(1)).await.unwrap();
    let keys = [
        "a.txt",
        "b/1",
        "b/2",
        "b/sub/3",
        "c/1",
        "d",
        "日本/x",
        "photos/2024/1.jpg",
        "photos/2024/2.jpg",
        "photos/2025/1.jpg",
        "photos/top.jpg",
    ];
    for k in keys {
        store
            .put_object(OWNER, put("lst", k, None), &b"x"[..])
            .await
            .unwrap();
    }

    // Flat listing is in UTF-8 byte order, across page boundaries.
    for use_cursor in [true, false] {
        let (all, cps, pages) = list_all(&store, "", None, 3, use_cursor).await;
        let mut expected: Vec<_> = keys.iter().map(|k| k.to_string()).collect();
        expected.sort();
        assert_eq!(all, expected);
        assert!(cps.is_empty());
        assert_eq!(pages, 4);
    }

    // Delimiter roll-up, with page size 1 so every prefix is its own page.
    for use_cursor in [true, false] {
        let (k, cps, _) = list_all(&store, "", Some("/"), 1, use_cursor).await;
        assert_eq!(k, vec!["a.txt", "d"]);
        assert_eq!(cps, vec!["b/", "c/", "photos/", "日本/"]);
    }

    let (k, cps, _) = list_all(&store, "photos/", Some("/"), 1000, true).await;
    assert_eq!(k, vec!["photos/top.jpg"]);
    assert_eq!(cps, vec!["photos/2024/", "photos/2025/"]);

    let (k, cps, _) = list_all(&store, "photos/2024/", None, 1000, true).await;
    assert_eq!(k, vec!["photos/2024/1.jpg", "photos/2024/2.jpg"]);
    assert!(cps.is_empty());

    let (k, _, _) = list_all(&store, "zzz", None, 1000, true).await;
    assert!(k.is_empty());
}

fn part_data(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
        .collect()
}

#[tokio::test]
async fn multipart_upload_assembles_one_plain_file_per_disk() {
    use suhms_store::{CompletedPart, MIN_PART_SIZE};
    let env = Env::new(3).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "mp1", None).await.unwrap();
    let mut req = put("mp1", "big.bin", None);
    req.user_metadata.insert("origin".into(), "test".into());
    let upload = store.create_multipart_upload(OWNER, req).await.unwrap();
    assert_eq!(upload.disks.len(), 3);

    let p1 = part_data(1, MIN_PART_SIZE as usize);
    let p2_old = part_data(2, 1000);
    let p2 = part_data(3, 1234);
    // Out of order, and part 2 re-uploaded: the later upload wins.
    let up = |n: u32, data: Vec<u8>| {
        let store = &store;
        async move {
            store
                .upload_part(OWNER, "mp1", "big.bin", upload.id, n, None, &data[..])
                .await
                .unwrap()
        }
    };
    up(2, p2_old).await;
    let r2 = up(2, p2.clone()).await;
    let r1 = up(1, p1.clone()).await;

    let (_, page) = store
        .list_parts(OWNER, "mp1", "big.bin", upload.id, 0, 1000)
        .await
        .unwrap();
    assert_eq!(page.parts.len(), 2);

    let parts = vec![
        CompletedPart {
            part_number: 1,
            etag: format!("\"{}\"", r1.etag),
        },
        CompletedPart {
            part_number: 2,
            etag: r2.etag.clone(),
        },
    ];
    let rec = store
        .complete_multipart_upload(OWNER, "mp1", "big.bin", upload.id, &parts)
        .await
        .unwrap();
    let mut expected = p1.clone();
    expected.extend_from_slice(&p2);
    assert_eq!(rec.size, expected.len() as u64);
    assert!(rec.etag.ends_with("-2"), "{}", rec.etag);
    assert_eq!(
        rec.user_metadata.get("origin").map(String::as_str),
        Some("test")
    );

    let locs = store.object_locations("mp1", "big.bin").await.unwrap();
    assert_eq!(locs.len(), 3);
    for p in &locs {
        assert_eq!(
            std::fs::read(p).unwrap(),
            expected,
            "a plain, complete file"
        );
    }
    for d in &env.disks {
        let dir = d
            .join(".suhms/multipart")
            .join(upload.id.simple().to_string());
        assert!(!dir.exists(), "part files are cleaned up");
    }
    assert_eq!(read_object(&store, "mp1", "big.bin").await, expected);
    // Scrub verifies the assembled files against the recorded hash.
    assert_eq!(store.scrub().await.unwrap().removed, 0);

    let err = store
        .complete_multipart_upload(OWNER, "mp1", "big.bin", upload.id, &parts)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NoSuchUpload), "{err}");
}

#[tokio::test]
async fn multipart_completion_is_validated() {
    use suhms_store::CompletedPart;
    let env = Env::new(1).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "mp1", Some(1)).await.unwrap();
    let upload = store
        .create_multipart_upload(OWNER, put("mp1", "k", None))
        .await
        .unwrap();
    let small1 = store
        .upload_part(OWNER, "mp1", "k", upload.id, 1, None, &b"aaa"[..])
        .await
        .unwrap();
    let small2 = store
        .upload_part(OWNER, "mp1", "k", upload.id, 2, None, &b"bbb"[..])
        .await
        .unwrap();
    let cp = |n: u32, etag: &str| CompletedPart {
        part_number: n,
        etag: etag.into(),
    };

    let complete = |parts: Vec<CompletedPart>| {
        let store = &store;
        async move {
            store
                .complete_multipart_upload(OWNER, "mp1", "k", upload.id, &parts)
                .await
                .unwrap_err()
        }
    };
    assert!(matches!(
        complete(vec![cp(3, "x")]).await,
        StoreError::InvalidPart(_)
    ));
    assert!(matches!(
        complete(vec![cp(1, "wrong")]).await,
        StoreError::InvalidPart(_)
    ));
    assert!(matches!(
        complete(vec![cp(2, &small2.etag), cp(1, &small1.etag)]).await,
        StoreError::InvalidPartOrder
    ));
    assert!(matches!(
        complete(vec![cp(1, &small1.etag), cp(2, &small2.etag)]).await,
        StoreError::EntityTooSmall
    ));
    let err = store
        .upload_part(OWNER, "mp1", "k", upload.id, 10_001, None, &b""[..])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidArgument(_)), "{err}");
    let err = store
        .upload_part(OWNER, "mp1", "other-key", upload.id, 1, None, &b""[..])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NoSuchUpload), "{err}");

    // A single (small) last part is fine.
    let rec = store
        .complete_multipart_upload(OWNER, "mp1", "k", upload.id, &[cp(2, &small2.etag)])
        .await
        .unwrap();
    assert_eq!(read_object(&store, "mp1", "k").await, b"bbb");
    assert!(rec.etag.ends_with("-1"));
}

#[tokio::test]
async fn abort_and_bucket_delete_clean_up_uploads() {
    let env = Env::new(2).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "mp1", Some(2)).await.unwrap();
    let u1 = store
        .create_multipart_upload(OWNER, put("mp1", "a", None))
        .await
        .unwrap();
    let u2 = store
        .create_multipart_upload(OWNER, put("mp1", "b", None))
        .await
        .unwrap();
    for u in [&u1, &u2] {
        store
            .upload_part(OWNER, "mp1", &u.key, u.id, 1, None, &b"part"[..])
            .await
            .unwrap();
    }
    let page = store
        .list_multipart_uploads(OWNER, "mp1", "", "", "", 1)
        .await
        .unwrap();
    assert_eq!(page.uploads.len(), 1);
    assert!(page.truncated);

    store
        .abort_multipart_upload(OWNER, "mp1", "a", u1.id)
        .await
        .unwrap();
    let dir = |u: &suhms_store::UploadRecord, d: &PathBuf| {
        d.join(".suhms/multipart").join(u.id.simple().to_string())
    };
    assert!(env.disks.iter().all(|d| !dir(&u1, d).exists()));
    assert!(env.disks.iter().all(|d| dir(&u2, d).exists()));

    // Deleting the (object-less) bucket aborts the remaining upload.
    store.delete_bucket(OWNER, "mp1").await.unwrap();
    assert!(env.disks.iter().all(|d| !dir(&u2, d).exists()));
}

#[tokio::test]
async fn copy_object_and_conditional_put() {
    let env = Env::new(2).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "src1", Some(2)).await.unwrap();
    store.create_bucket(OWNER, "dst1", Some(1)).await.unwrap();
    let mut req = put("src1", "a", None);
    req.user_metadata.insert("k".into(), "v".into());
    req.content_type = Some("text/plain".into());
    store.put_object(OWNER, req, &b"hello"[..]).await.unwrap();

    let rec = store
        .copy_object(OWNER, "src1", "a", put("dst1", "b", None), false)
        .await
        .unwrap();
    assert_eq!(rec.user_metadata.get("k").map(String::as_str), Some("v"));
    assert_eq!(rec.content_type.as_deref(), Some("text/plain"));
    assert_eq!(read_object(&store, "dst1", "b").await, b"hello");
    assert_eq!(store.object_locations("dst1", "b").await.unwrap().len(), 1);

    let rec = store
        .copy_object(OWNER, "src1", "a", put("dst1", "c", None), true)
        .await
        .unwrap();
    assert!(rec.user_metadata.is_empty());

    let err = store
        .copy_object(OWNER, "src1", "a", put("src1", "a", None), false)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidRequest(_)), "{err}");

    let mut req = put("src1", "a", None);
    req.if_absent = true;
    let err = store.put_object(OWNER, req, &b"x"[..]).await.unwrap_err();
    assert!(matches!(err, StoreError::PreconditionFailed), "{err}");
    let mut req = put("src1", "new", None);
    req.if_absent = true;
    store.put_object(OWNER, req, &b"x"[..]).await.unwrap();
}

#[tokio::test]
async fn quotas_and_bucket_limits() {
    use suhms_store::TenantUpdate;
    let env = Env::new(1).await;
    let store = env.open().await;
    let t = store.create_tenant("limited").await.unwrap();
    store
        .metadata()
        .update_tenant(
            &t.id,
            &TenantUpdate {
                quota_bytes: Some(Some(10)),
                max_buckets: Some(Some(1)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let got = store.metadata().get_tenant(&t.id).await.unwrap().unwrap();
    assert_eq!(
        (got.quota_bytes, got.max_buckets, got.suspended),
        (Some(10), Some(1), false)
    );

    store.create_bucket(&t.id, "only", Some(1)).await.unwrap();
    let err = store
        .create_bucket(&t.id, "second", Some(1))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::TooManyBuckets), "{err}");

    store
        .put_object(&t.id, put("only", "a", None), &b"123456"[..])
        .await
        .unwrap();
    let err = store
        .put_object(&t.id, put("only", "b", None), &b"123456"[..])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::QuotaExceeded), "{err}");
    let err = store.get_object(&t.id, "only", "b").await.unwrap_err();
    assert!(
        matches!(err, StoreError::NoSuchKey { .. }),
        "rejected write left nothing: {err}"
    );

    // Lifting the quota takes effect.
    store
        .metadata()
        .update_tenant(
            &t.id,
            &TenantUpdate {
                quota_bytes: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    store
        .put_object(&t.id, put("only", "b", None), &b"123456"[..])
        .await
        .unwrap();
}

#[tokio::test]
async fn public_buckets_serve_anonymous_reads_only_when_marked() {
    let env = Env::new(1).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "pub", Some(1)).await.unwrap();
    store
        .put_object(OWNER, put("pub", "x", None), &b"hi"[..])
        .await
        .unwrap();
    let err = store.get_public_object("pub", "x").await.unwrap_err();
    assert!(matches!(err, StoreError::AccessDenied), "{err}");
    store.set_bucket_public("pub", true).await.unwrap();
    let stored = store.get_public_object("pub", "x").await.unwrap();
    assert_eq!(stored.object.size, 2);
    assert!(store.get_bucket(OWNER, "pub").await.unwrap().public_read);
}

#[tokio::test]
async fn stale_uploads_are_aborted() {
    let env = Env::new(1).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "mpu", Some(1)).await.unwrap();
    let u = store
        .create_multipart_upload(OWNER, put("mpu", "k", None))
        .await
        .unwrap();
    store
        .upload_part(OWNER, "mpu", "k", u.id, 1, None, &b"part"[..])
        .await
        .unwrap();
    assert_eq!(
        store
            .abort_stale_uploads(std::time::Duration::from_secs(3600))
            .await
            .unwrap(),
        0
    );
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(
        store
            .abort_stale_uploads(std::time::Duration::from_millis(1))
            .await
            .unwrap(),
        1
    );
    let dir = env.disks[0]
        .join(".suhms/multipart")
        .join(u.id.simple().to_string());
    assert!(!dir.exists());
}

#[tokio::test]
async fn usage_meter_accumulates_and_snapshots() {
    use suhms_store::{UsageDelta, hour_of};
    let env = Env::new(1).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "met", Some(1)).await.unwrap();
    store
        .put_object(OWNER, put("met", "a", None), &b"12345"[..])
        .await
        .unwrap();
    store
        .put_object(OWNER, put("met", "b", None), &b"123"[..])
        .await
        .unwrap();

    let hour = hour_of(1_700_000_123_456);
    assert_eq!(hour, 1_699_999_200_000);
    let delta = UsageDelta {
        tenant_id: OWNER.into(),
        hour,
        class_a: 2,
        class_b: 5,
        ingress_bytes: 100,
        egress_bytes: 7,
    };
    store
        .metadata()
        .add_usage(&[delta.clone(), delta])
        .await
        .unwrap();
    let rows = store
        .metadata()
        .usage(OWNER, hour, hour + 3_600_000)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].class_a_requests,
            rows[0].class_b_requests,
            rows[0].ingress_bytes,
            rows[0].egress_bytes
        ),
        (4, 10, 200, 14)
    );

    let snap = store.snapshot_storage().await.unwrap();
    let mine = snap.iter().find(|u| u.tenant_id == OWNER).unwrap();
    assert_eq!(
        (mine.objects, mine.logical_bytes, mine.stored_bytes),
        (2, 8, 8)
    );
    let now = hour_of(
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap(),
    );
    let rows = store
        .metadata()
        .usage(OWNER, now, now + 3_600_000)
        .await
        .unwrap();
    assert_eq!(rows[0].logical_bytes, 8);
    // Snapshots overwrite, they do not add up.
    store.snapshot_storage().await.unwrap();
    let rows = store
        .metadata()
        .usage(OWNER, now, now + 3_600_000)
        .await
        .unwrap();
    assert_eq!((rows[0].stored_objects, rows[0].logical_bytes), (2, 8));
}

async fn versions(store: &Store, bucket: &str) -> Vec<(String, String, bool, bool)> {
    store
        .list_object_versions(OWNER, bucket, "", "", None, 1000)
        .await
        .unwrap()
        .versions
        .into_iter()
        .map(|o| (o.key, o.version_id, o.is_latest, o.delete_marker))
        .collect()
}

#[tokio::test]
async fn versioning_keeps_old_versions_and_delete_markers() {
    let env = Env::new(1).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "ver", Some(1)).await.unwrap();
    store.set_versioning(OWNER, "ver", "Enabled").await.unwrap();

    let v1 = store
        .put_object(OWNER, put("ver", "k", None), &b"one"[..])
        .await
        .unwrap();
    let v2 = store
        .put_object(OWNER, put("ver", "k", None), &b"two"[..])
        .await
        .unwrap();
    assert_ne!(v1.version_id, "null");
    assert_ne!(v1.version_id, v2.version_id);
    assert_eq!(read_object(&store, "ver", "k").await, b"two");
    let old = store
        .get_object_version(OWNER, "ver", "k", &v1.version_id)
        .await
        .unwrap();
    let mut r = store.read_object(&old, 0..3).await.unwrap();
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).await.unwrap();
    assert_eq!(buf, b"one");

    // Delete adds a marker; the data stays.
    let d = store.delete_object(OWNER, "ver", "k").await.unwrap();
    assert!(d.delete_marker);
    let err = store.get_object(OWNER, "ver", "k").await.unwrap_err();
    assert!(matches!(err, StoreError::NoSuchKey { .. }), "{err}");
    let page = store
        .list_objects(
            OWNER,
            &ListParams {
                bucket: "ver".into(),
                prefix: String::new(),
                delimiter: None,
                start: ListStart::Beginning,
                max_keys: 1000,
            },
        )
        .await
        .unwrap();
    assert!(page.objects.is_empty(), "deleted keys are not listed");
    let vs = versions(&store, "ver").await;
    assert_eq!(vs.len(), 3);
    assert!(
        vs[0].2 && vs[0].3,
        "newest is the latest, a delete marker: {vs:?}"
    );
    let err = store
        .get_object_version(OWNER, "ver", "k", d.version_id.as_deref().unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::MethodNotAllowed), "{err}");

    // Removing the marker restores the key; removing v2 makes v1 current.
    store
        .delete_object_version(OWNER, "ver", "k", d.version_id.as_deref().unwrap(), false)
        .await
        .unwrap();
    assert_eq!(read_object(&store, "ver", "k").await, b"two");
    store
        .delete_object_version(OWNER, "ver", "k", &v2.version_id, false)
        .await
        .unwrap();
    assert_eq!(read_object(&store, "ver", "k").await, b"one");
    assert_eq!(versions(&store, "ver").await.len(), 1);
    let err = store.delete_bucket(OWNER, "ver").await.unwrap_err();
    assert!(matches!(err, StoreError::BucketNotEmpty(_)), "{err}");
}

#[tokio::test]
async fn suspended_versioning_replaces_only_the_null_version() {
    let env = Env::new(1).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "sus", Some(1)).await.unwrap();
    store
        .put_object(OWNER, put("sus", "k", None), &b"pre"[..])
        .await
        .unwrap(); // null version
    store.set_versioning(OWNER, "sus", "Enabled").await.unwrap();
    let kept = store
        .put_object(OWNER, put("sus", "k", None), &b"kept"[..])
        .await
        .unwrap();
    store
        .set_versioning(OWNER, "sus", "Suspended")
        .await
        .unwrap();
    store
        .put_object(OWNER, put("sus", "k", None), &b"a"[..])
        .await
        .unwrap();
    store
        .put_object(OWNER, put("sus", "k", None), &b"b"[..])
        .await
        .unwrap();
    let vs = versions(&store, "sus").await;
    let ids: Vec<&str> = vs.iter().map(|v| v.1.as_str()).collect();
    assert_eq!(ids, vec!["null", kept.version_id.as_str()], "{vs:?}");
    assert_eq!(read_object(&store, "sus", "k").await, b"b");
    // Only the live files remain on disk (the replaced null versions were removed).
    let files = walk_files(&env.disks[0].join("objects"));
    assert_eq!(files, 2);
}

fn walk_files(p: &std::path::Path) -> usize {
    std::fs::read_dir(p).map_or(0, |entries| {
        entries
            .flatten()
            .map(|e| {
                if e.path().is_dir() {
                    walk_files(&e.path())
                } else {
                    1
                }
            })
            .sum()
    })
}

#[tokio::test]
async fn object_lock_protects_versions() {
    let env = Env::new(1).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "worm", Some(1)).await.unwrap();
    store.enable_object_lock(OWNER, "worm").await.unwrap();
    let err = store
        .set_versioning(OWNER, "worm", "Suspended")
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidRequest(_)), "{err}");
    store
        .set_default_retention(OWNER, "worm", Some(("GOVERNANCE".into(), 1)))
        .await
        .unwrap();

    let g = store
        .put_object(OWNER, put("worm", "gov", None), &b"g"[..])
        .await
        .unwrap();
    assert_eq!(g.lock_mode.as_deref(), Some("GOVERNANCE"));
    let err = store
        .delete_object_version(OWNER, "worm", "gov", &g.version_id, false)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::ObjectLocked), "{err}");
    store
        .delete_object_version(OWNER, "worm", "gov", &g.version_id, true)
        .await
        .unwrap();

    let far = 4_102_444_800_000; // 2100-01-01
    let mut req = put("worm", "comp", None);
    req.lock_mode = Some("COMPLIANCE".into());
    req.lock_until = Some(far);
    let c = store.put_object(OWNER, req, &b"c"[..]).await.unwrap();
    let err = store
        .delete_object_version(OWNER, "worm", "comp", &c.version_id, true)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ObjectLocked),
        "compliance cannot be bypassed: {err}"
    );
    let err = store
        .set_object_retention(
            OWNER,
            "worm",
            "comp",
            None,
            Some(("COMPLIANCE".into(), far - 1)),
            true,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ObjectLocked),
        "compliance cannot be shortened: {err}"
    );
    store
        .set_object_retention(
            OWNER,
            "worm",
            "comp",
            None,
            Some(("COMPLIANCE".into(), far + 1)),
            false,
        )
        .await
        .unwrap();
    // Deleting the key itself only adds a marker, which is allowed.
    store.delete_object(OWNER, "worm", "comp").await.unwrap();

    let h = store
        .put_object(OWNER, put("worm", "held", None), &b"h"[..])
        .await
        .unwrap();
    store
        .set_object_retention(OWNER, "worm", "held", None, None, true)
        .await
        .unwrap();
    store
        .set_legal_hold(OWNER, "worm", "held", None, true)
        .await
        .unwrap();
    let err = store
        .delete_object_version(OWNER, "worm", "held", &h.version_id, true)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::ObjectLocked), "{err}");
    store
        .set_legal_hold(OWNER, "worm", "held", None, false)
        .await
        .unwrap();
    store
        .delete_object_version(OWNER, "worm", "held", &h.version_id, false)
        .await
        .unwrap();
}

#[tokio::test]
async fn lifecycle_expires_and_purges() {
    use suhms_store::lifecycle::LifecycleRule;
    let env = Env::new(1).await;
    let store = env.open().await;
    store.create_bucket(OWNER, "lcy", Some(1)).await.unwrap();
    store.set_versioning(OWNER, "lcy", "Enabled").await.unwrap();
    for k in ["logs/a", "logs/b", "keep/c"] {
        store
            .put_object(OWNER, put("lcy", k, None), &b"x"[..])
            .await
            .unwrap();
    }
    let rules = vec![LifecycleRule {
        id: "logs".into(),
        prefix: "logs/".into(),
        enabled: true,
        expiration_days: Some(0),
        noncurrent_days: None,
        abort_multipart_days: None,
    }];
    store
        .set_lifecycle(OWNER, "lcy", Some(&rules))
        .await
        .unwrap();
    assert_eq!(store.lifecycle(OWNER, "lcy").await.unwrap().unwrap(), rules);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let (expired, purged, _) = store.apply_lifecycle().await.unwrap();
    assert_eq!((expired, purged), (2, 0));
    assert!(store.get_object(OWNER, "lcy", "logs/a").await.is_err());
    assert_eq!(read_object(&store, "lcy", "keep/c").await, b"x");

    // Now purge the noncurrent data of expired keys.
    let mut rules = rules;
    rules[0].noncurrent_days = Some(0);
    store
        .set_lifecycle(OWNER, "lcy", Some(&rules))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let (_, purged, _) = store.apply_lifecycle().await.unwrap();
    assert_eq!(purged, 2);
    let vs = versions(&store, "lcy").await;
    assert!(
        vs.iter().filter(|v| v.0.starts_with("logs/")).all(|v| v.3),
        "only markers left: {vs:?}"
    );
}
