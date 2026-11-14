//! Checksums added to objects stored without one.

mod common;

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use common::Server;
use s3s::crypto::{Checksum as _, Crc64Nvme};
use suhms_server::backfill::{BackfillOptions, backfill_checksums};
use suhms_store::PutObjectRequest;

/// Writes an object the way it was stored before checksums were kept (no checksum).
async fn put_legacy(srv: &Server, owner: &str, key: &str, data: &[u8]) {
    srv.store
        .put_object(
            owner,
            PutObjectRequest {
                bucket: "legacy".into(),
                key: key.into(),
                ..Default::default()
            },
            data,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn backfill_adds_crc64nvme_to_objects_without_a_checksum() {
    let srv = Server::start(1).await;
    let (owner, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/legacy", &[], &[], vec![]).await;
    let data = vec![7u8; 3 * 1024 * 1024 + 5];
    put_legacy(&srv, &owner, "old.bin", &data).await;
    put_legacy(&srv, &owner, "rotten.bin", b"will be corrupted").await;
    // A normal upload already has one and is left alone.
    srv.s3(&c, "PUT", "/legacy/new.txt", &[], &[], b"new".to_vec())
        .await;

    let mode = [("x-amz-checksum-mode", "ENABLED")];
    let head = srv
        .s3(&c, "HEAD", "/legacy/old.bin", &[], &mode, vec![])
        .await;
    assert_eq!(head.header("x-amz-checksum-crc64nvme"), None);

    // Corrupt the only copy of one object: it must not get a checksum.
    let path = srv
        .store
        .object_locations("legacy", "rotten.bin")
        .await
        .unwrap()
        .remove(0);
    std::fs::write(&path, b"WILL BE CORRUPTED").unwrap();

    let r = backfill_checksums(&srv.store, BackfillOptions::default())
        .await
        .unwrap();
    assert_eq!((r.updated, r.corrupt, r.failed), (1, 1, 0), "{r:?}");

    let expected = B64.encode(Crc64Nvme::checksum(&data));
    for method in ["HEAD", "GET"] {
        let resp = srv
            .s3(&c, method, "/legacy/old.bin", &[], &mode, vec![])
            .await;
        assert_eq!(
            resp.header("x-amz-checksum-crc64nvme"),
            Some(expected.as_str())
        );
        assert_eq!(resp.header("x-amz-checksum-type"), Some("FULL_OBJECT"));
    }
    let rotten = srv
        .s3(&c, "HEAD", "/legacy/rotten.bin", &[], &mode, vec![])
        .await;
    assert_eq!(rotten.header("x-amz-checksum-crc64nvme"), None);

    // Fixed copy: the next run picks it up; after that there is nothing left.
    std::fs::write(&path, b"will be corrupted").unwrap();
    let r = backfill_checksums(&srv.store, BackfillOptions::default())
        .await
        .unwrap();
    assert_eq!(r.updated, 1, "{r:?}");
    let r = backfill_checksums(&srv.store, BackfillOptions::default())
        .await
        .unwrap();
    assert!(r.is_idle(), "{r:?}");
}

#[tokio::test]
async fn backfill_stops_at_max_objects() {
    let srv = Server::start(1).await;
    let (owner, c) = srv.tenant("bob").await;
    srv.s3(&c, "PUT", "/legacy", &[], &[], vec![]).await;
    for k in ["a", "b", "c"] {
        put_legacy(&srv, &owner, k, k.as_bytes()).await;
    }
    let options = BackfillOptions {
        max_objects: Some(2),
        bytes_per_sec: Some(1 << 20),
    };
    let r = backfill_checksums(&srv.store, options).await.unwrap();
    assert_eq!((r.updated, r.remaining), (2, true));
    let r = backfill_checksums(&srv.store, options).await.unwrap();
    assert_eq!((r.updated, r.remaining), (1, false));
}
