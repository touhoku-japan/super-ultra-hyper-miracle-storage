mod common;

use axum::{body::Body, http::Request};
use common::{Creds, Server, path_encode, signed_with_payload_hash, xml_values};
use futures::StreamExt;
use md5::{Digest, Md5};

fn data(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

#[tokio::test]
async fn health_needs_no_auth() {
    let srv = Server::start(1).await;
    let resp = srv
        .send(
            Request::get(suhms_server::HEALTH_PATH)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.text(), "ok");

    // The old path is gone (it is now just an unauthenticated S3 request).
    let resp = srv
        .send(Request::get("/suhms/health").body(Body::empty()).unwrap())
        .await;
    assert_ne!(resp.status, 200);
}

#[tokio::test]
async fn health_fails_while_no_disk_is_usable() {
    let srv = Server::start(1).await;
    let health = || {
        Request::get(suhms_server::HEALTH_PATH)
            .body(Body::empty())
            .unwrap()
    };
    // The only disk is unmounted: the next heartbeat's probe takes it out of service.
    let away = srv.disks[0].with_extension("away");
    std::fs::rename(&srv.disks[0], &away).unwrap();
    srv.store.heartbeat().await.unwrap();
    let resp = srv.send(health()).await;
    assert_eq!(resp.status, 503);
    assert_eq!(resp.text(), "no healthy disks");

    std::fs::rename(&away, &srv.disks[0]).unwrap();
    srv.store.heartbeat().await.unwrap();
    // The result is reused for a second.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let resp = srv.send(health()).await;
    assert_eq!(resp.status, 200);
}

/// A request body that records how many bytes the server read from it.
fn counted_body(chunks: usize) -> (Body, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let read = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&read);
    let stream = futures::stream::iter(0..chunks).map(move |_| {
        counter.fetch_add(4096, std::sync::atomic::Ordering::SeqCst);
        Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![b'x'; 4096]))
    });
    (Body::from_stream(stream), read)
}

#[tokio::test]
async fn a_rejected_request_body_is_read_before_answering() {
    use std::sync::atomic::Ordering;
    let srv = Server::start(1).await;
    // Anonymous PUT: refused without the service reading the body. The rest is read anyway so
    // the client sees the 403 instead of a reset connection.
    let (body, read) = counted_body(16);
    let resp = srv
        .send(
            Request::put("/some-bucket/key")
                .header("content-length", 16 * 4096)
                .body(body)
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status, 403);
    assert_eq!(read.load(Ordering::SeqCst), 16 * 4096);

    // A client waiting for 100 Continue has sent nothing: the body is left alone.
    let (body, read) = counted_body(16);
    let resp = srv
        .send(
            Request::put("/some-bucket/key")
                .header("content-length", 16 * 4096)
                .header("expect", "100-continue")
                .body(body)
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status, 403);
    assert_eq!(read.load(Ordering::SeqCst), 0);

    // Large bodies are not read (at most DRAIN_MAX, then the connection is closed).
    let (body, read) = counted_body(1024);
    let resp = srv
        .send(Request::put("/some-bucket/key").body(body).unwrap())
        .await;
    assert_eq!(resp.status, 403);
    let n = read.load(Ordering::SeqCst) as u64;
    assert!(n <= suhms_server::drain::DRAIN_MAX + 4096, "{n}");
}

#[tokio::test]
async fn reserved_bucket_name_is_rejected() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    for name in ["/super-ultra-hyper-miracle-storage", "/suhms"] {
        let resp = srv.s3(&c, "PUT", name, &[], &[], vec![]).await;
        assert_eq!(
            resp.error_code().as_deref(),
            Some("InvalidBucketName"),
            "{name}"
        );
    }
}

#[tokio::test]
async fn rejects_anonymous_and_badly_signed_requests() {
    let srv = Server::start(1).await;
    let (_, creds) = srv.tenant("alice").await;

    let resp = srv
        .send(Request::get("/").body(Body::empty()).unwrap())
        .await;
    assert_eq!(resp.status, 403, "{}", resp.text());

    let wrong = Creds {
        key: creds.key.clone(),
        secret: "x".repeat(40),
    };
    let resp = srv.s3(&wrong, "GET", "/", &[], &[], vec![]).await;
    assert_eq!(resp.status, 403);
    assert_eq!(resp.error_code().as_deref(), Some("SignatureDoesNotMatch"));

    let unknown = Creds {
        key: "SUNOSUCHKEY000000000".into(),
        secret: creds.secret.clone(),
    };
    let resp = srv.s3(&unknown, "GET", "/", &[], &[], vec![]).await;
    assert_eq!(resp.status, 403);
    assert_eq!(resp.error_code().as_deref(), Some("InvalidAccessKeyId"));

    // A disabled key stops working immediately.
    assert_eq!(
        srv.s3(&creds, "GET", "/", &[], &[], vec![]).await.status,
        200
    );
    let (status, _) = srv
        .admin(
            "PATCH",
            &format!("/admin/v1/keys/{}", creds.key),
            Some(serde_json::json!({"active": false})),
        )
        .await;
    assert_eq!(status, 204);
    let resp = srv.s3(&creds, "GET", "/", &[], &[], vec![]).await;
    assert_eq!(resp.error_code().as_deref(), Some("InvalidAccessKeyId"));
}

#[tokio::test]
async fn object_round_trip() {
    let srv = Server::start(3).await;
    let (_, c) = srv.tenant("alice").await;

    let resp = srv.s3(&c, "PUT", "/photos", &[], &[], vec![]).await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    assert_eq!(resp.header("location"), Some("/photos"));

    let body = data(300_000);
    let key = "2024/夏/海 #1.jpg";
    let path = format!("/photos/{}", path_encode(key));
    let resp = srv
        .s3(
            &c,
            "PUT",
            &path,
            &[],
            &[
                ("content-type", "image/jpeg"),
                ("x-amz-meta-camera", "x100"),
            ],
            body.clone(),
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let etag = format!("\"{}\"", hex::encode(Md5::digest(&body)));
    assert_eq!(resp.header("etag"), Some(etag.as_str()));

    let resp = srv.s3(&c, "GET", &path, &[], &[], vec![]).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body.as_ref(), body.as_slice());
    assert_eq!(resp.header("content-type"), Some("image/jpeg"));
    assert_eq!(resp.header("x-amz-meta-camera"), Some("x100"));
    assert_eq!(resp.header("etag"), Some(etag.as_str()));
    assert!(resp.header("last-modified").is_some());

    let resp = srv
        .s3(&c, "GET", &path, &[], &[("range", "bytes=100-199")], vec![])
        .await;
    assert_eq!(resp.status, 206);
    assert_eq!(resp.body.as_ref(), &body[100..200]);
    assert_eq!(resp.header("content-range"), Some("bytes 100-199/300000"));

    let resp = srv
        .s3(&c, "GET", &path, &[], &[("range", "bytes=-10")], vec![])
        .await;
    assert_eq!(resp.body.as_ref(), &body[body.len() - 10..]);

    let resp = srv
        .s3(&c, "GET", &path, &[], &[("range", "bytes=999999-")], vec![])
        .await;
    assert_eq!(resp.status, 416);

    let resp = srv.s3(&c, "HEAD", &path, &[], &[], vec![]).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("content-length"), Some("300000"));

    // Copies are plain files on three distinct disks.
    let locs = srv.store.object_locations("photos", key).await.unwrap();
    assert_eq!(locs.len(), 3);
    for p in &locs {
        assert_eq!(std::fs::read(p).unwrap(), body);
    }

    let resp = srv.s3(&c, "DELETE", &path, &[], &[], vec![]).await;
    assert_eq!(resp.status, 204);
    let resp = srv.s3(&c, "GET", &path, &[], &[], vec![]).await;
    assert_eq!(resp.status, 404);
    assert_eq!(resp.error_code().as_deref(), Some("NoSuchKey"));
    assert!(locs.iter().all(|p| !p.exists()));

    let resp = srv.s3(&c, "DELETE", "/photos", &[], &[], vec![]).await;
    assert_eq!(resp.status, 204);
    let resp = srv.s3(&c, "GET", "/photos/x", &[], &[], vec![]).await;
    assert_eq!(resp.error_code().as_deref(), Some("NoSuchBucket"));
}

#[tokio::test]
async fn storage_class_selects_replica_count() {
    let srv = Server::start(3).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/bk1", &[], &[], vec![]).await;
    for (class, n) in [
        ("REDUCED_REDUNDANCY", 1),
        ("STANDARD_IA", 2),
        ("STANDARD", 3),
    ] {
        let path = format!("/bk1/{class}");
        let resp = srv
            .s3(
                &c,
                "PUT",
                &path,
                &[],
                &[("x-amz-storage-class", class)],
                b"x".to_vec(),
            )
            .await;
        assert_eq!(resp.status, 200, "{}", resp.text());
        assert_eq!(
            srv.store
                .object_locations("bk1", class)
                .await
                .unwrap()
                .len(),
            n
        );
    }
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/bk1/g",
            &[],
            &[("x-amz-storage-class", "GLACIER")],
            b"x".to_vec(),
        )
        .await;
    assert_eq!(resp.error_code().as_deref(), Some("InvalidStorageClass"));
}

#[tokio::test]
async fn integrity_checks_on_upload() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/bk1", &[], &[], vec![]).await;

    // Body does not match the signed x-amz-content-sha256.
    let req = signed_with_payload_hash(
        &c,
        "PUT",
        "/bk1/k",
        &[],
        &[],
        b"tampered".to_vec().into(),
        &hex::encode(sha2::Sha256::digest(b"original")),
    );
    let resp = srv.send(req).await;
    assert_eq!(resp.status, 400, "{}", resp.text());
    assert_eq!(
        resp.error_code().as_deref(),
        Some("XAmzContentSHA256Mismatch")
    );
    let resp = srv.s3(&c, "HEAD", "/bk1/k", &[], &[], vec![]).await;
    assert_eq!(resp.status, 404, "nothing was stored");

    // Wrong Content-MD5.
    let wrong_md5 = base64_md5(b"something else");
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/bk1/k",
            &[],
            &[("content-md5", &wrong_md5)],
            b"body".to_vec(),
        )
        .await;
    assert_eq!(resp.error_code().as_deref(), Some("BadDigest"));
    let resp = srv.s3(&c, "HEAD", "/bk1/k", &[], &[], vec![]).await;
    assert_eq!(resp.status, 404);

    let good = base64_md5(b"body");
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/bk1/k",
            &[],
            &[("content-md5", &good)],
            b"body".to_vec(),
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
}

fn base64_md5(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(Md5::digest(data))
}

#[tokio::test]
async fn tenants_are_isolated() {
    let srv = Server::start(1).await;
    let (_, alice) = srv.tenant("alice").await;
    let (_, bob) = srv.tenant("bob").await;

    srv.s3(&alice, "PUT", "/alice-data", &[], &[], vec![]).await;
    srv.s3(&alice, "PUT", "/alice-data/secret", &[], &[], b"s".to_vec())
        .await;

    for (method, path, query) in [
        ("GET", "/alice-data/secret", &[][..]),
        ("HEAD", "/alice-data/secret", &[][..]),
        ("PUT", "/alice-data/planted", &[][..]),
        ("DELETE", "/alice-data/secret", &[][..]),
        ("GET", "/alice-data", &[("list-type", "2")][..]),
        ("GET", "/alice-data", &[][..]),
        ("HEAD", "/alice-data", &[][..]),
        ("DELETE", "/alice-data", &[][..]),
    ] {
        let resp = srv.s3(&bob, method, path, query, &[], vec![]).await;
        assert_eq!(resp.status, 403, "{method} {path} {query:?}");
    }

    let resp = srv.s3(&bob, "PUT", "/alice-data", &[], &[], vec![]).await;
    assert_eq!(resp.error_code().as_deref(), Some("BucketAlreadyExists"));
    let resp = srv.s3(&alice, "PUT", "/alice-data", &[], &[], vec![]).await;
    assert_eq!(
        resp.error_code().as_deref(),
        Some("BucketAlreadyOwnedByYou")
    );

    let resp = srv.s3(&bob, "GET", "/", &[], &[], vec![]).await;
    assert!(xml_values(&resp.text(), "Name").is_empty());
    let resp = srv.s3(&alice, "GET", "/", &[], &[], vec![]).await;
    assert_eq!(xml_values(&resp.text(), "Name"), vec!["alice-data"]);

    let resp = srv
        .s3(&alice, "GET", "/alice-data/secret", &[], &[], vec![])
        .await;
    assert_eq!(resp.body.as_ref(), b"s");
}

#[tokio::test]
async fn listing_v1_v2_with_delimiter_and_url_encoding() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/lst", &[], &[], vec![]).await;
    for key in ["a+b c.txt", "dir/1", "dir/2", "dir/sub/3", "top"] {
        let resp = srv
            .s3(
                &c,
                "PUT",
                &format!("/lst/{}", path_encode(key)),
                &[],
                &[],
                b"x".to_vec(),
            )
            .await;
        assert_eq!(resp.status, 200, "{key}: {}", resp.text());
    }

    // V2, delimiter, paged one entry at a time via continuation tokens.
    let mut keys = Vec::new();
    let mut prefixes = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut q = vec![("list-type", "2"), ("delimiter", "/"), ("max-keys", "1")];
        if let Some(t) = &token {
            q.push(("continuation-token", t));
        }
        let resp = srv.s3(&c, "GET", "/lst", &q, &[], vec![]).await;
        assert_eq!(resp.status, 200, "{}", resp.text());
        let xml = resp.text();
        keys.extend(xml_values(&xml, "Key"));
        prefixes.extend(
            xml_values(&xml, "Prefix")
                .into_iter()
                .filter(|p| !p.is_empty()),
        );
        if xml_values(&xml, "IsTruncated") == ["false"] {
            break;
        }
        token = xml_values(&xml, "NextContinuationToken").into_iter().next();
        assert!(token.is_some());
    }
    assert_eq!(keys, vec!["a+b c.txt", "top"]);
    assert_eq!(prefixes, vec!["dir/"]);

    // encoding-type=url, as the AWS CLI sends it.
    let resp = srv
        .s3(
            &c,
            "GET",
            "/lst",
            &[("list-type", "2"), ("encoding-type", "url")],
            &[],
            vec![],
        )
        .await;
    let xml = resp.text();
    assert!(
        xml_values(&xml, "Key").contains(&"a%2Bb%20c.txt".to_owned()),
        "{xml}"
    );
    assert_eq!(xml_values(&xml, "KeyCount"), vec!["5"]);

    // V1 with marker paging.
    let mut marker = String::new();
    let mut all = Vec::new();
    loop {
        let resp = srv
            .s3(
                &c,
                "GET",
                "/lst",
                &[("marker", &marker), ("max-keys", "2"), ("prefix", "dir/")],
                &[],
                vec![],
            )
            .await;
        let xml = resp.text();
        let page = xml_values(&xml, "Key");
        all.extend(page.clone());
        if xml_values(&xml, "IsTruncated") == ["false"] {
            break;
        }
        marker = page.last().unwrap().clone();
    }
    assert_eq!(all, vec!["dir/1", "dir/2", "dir/sub/3"]);
}

#[tokio::test]
async fn delete_objects_batch() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/bk1", &[], &[], vec![]).await;
    for k in ["a", "b", "c"] {
        srv.s3(&c, "PUT", &format!("/bk1/{k}"), &[], &[], b"x".to_vec())
            .await;
    }
    let xml = "<Delete><Object><Key>a</Key></Object><Object><Key>b</Key></Object>\
               <Object><Key>missing</Key></Object></Delete>";
    let md5 = base64_md5(xml.as_bytes());
    let resp = srv
        .s3(
            &c,
            "POST",
            "/bk1",
            &[("delete", "")],
            &[("content-md5", &md5)],
            xml.as_bytes().to_vec(),
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let mut deleted = xml_values(&resp.text(), "Key");
    deleted.sort();
    assert_eq!(deleted, vec!["a", "b", "missing"]);

    let resp = srv
        .s3(&c, "GET", "/bk1", &[("list-type", "2")], &[], vec![])
        .await;
    assert_eq!(xml_values(&resp.text(), "Key"), vec!["c"]);
}

#[tokio::test]
async fn get_of_a_corrupt_copy_is_cut_short() {
    use tower::ServiceExt as _;
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("acme").await;
    srv.s3(&c, "PUT", "/bk1", &[], &[], vec![]).await;
    let body = data(300_000);
    let mut bad = body.clone();
    bad[1234] ^= 0xff;
    // Two objects: the ranged read schedules a background check that drops the corrupt (and
    // only) copy, after which the object could no longer be read at all.
    for key in ["k", "ranged"] {
        let path = format!("/bk1/{key}");
        assert_eq!(
            srv.s3(&c, "PUT", &path, &[], &[], body.clone())
                .await
                .status,
            200
        );
        let locs = srv.store.object_locations("bk1", key).await.unwrap();
        std::fs::write(&locs[0], &bad).unwrap();
    }

    // A ranged GET is not checked (that would mean reading the whole object).
    let resp = srv
        .s3(
            &c,
            "GET",
            "/bk1/ranged",
            &[],
            &[("range", "bytes=0-9")],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 206);
    assert_eq!(&resp.body[..], &bad[..10]);

    // Headers are already out when the mismatch shows up, so the body fails before its last
    // bytes: the client gets less than Content-Length (a broken transfer), never the corrupt
    // bytes as a finished download. An HTTP server stops polling the body once Content-Length
    // bytes are out, so only the bytes before the error count here.
    let req = common::signed(&c, "GET", "/bk1/k", &[], &[], Default::default());
    let resp = srv.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let mut stream = resp.into_body().into_data_stream();
    let mut received = 0;
    let mut failed = false;
    while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
        match chunk {
            Ok(bytes) => received += bytes.len(),
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    assert!(failed);
    assert!(received < body.len());
}

#[tokio::test]
async fn delete_objects_on_another_tenants_bucket_is_refused_as_a_whole() {
    let srv = Server::start(1).await;
    let (_, alice) = srv.tenant("alice").await;
    let (_, bob) = srv.tenant("bob").await;
    srv.s3(&alice, "PUT", "/alices", &[], &[], vec![]).await;
    srv.s3(&alice, "PUT", "/alices/a", &[], &[], b"x".to_vec())
        .await;
    let xml = "<Delete><Object><Key>a</Key></Object></Delete>";
    let md5 = base64_md5(xml.as_bytes());
    for (bucket, status, code) in [
        ("/alices", 403, "AccessDenied"),
        ("/nobody", 404, "NoSuchBucket"),
    ] {
        let resp = srv
            .s3(
                &bob,
                "POST",
                bucket,
                &[("delete", "")],
                &[("content-md5", &md5)],
                xml.as_bytes().to_vec(),
            )
            .await;
        assert_eq!(resp.status, status, "{}", resp.text());
        assert_eq!(resp.error_code().as_deref(), Some(code));
    }
    let resp = srv.s3(&alice, "GET", "/alices/a", &[], &[], vec![]).await;
    assert_eq!(resp.status, 200);
}

/// Keys with control characters (other than tab / LF / CR) cannot appear in an XML listing, so
/// writes that would create them are refused instead of breaking every later listing.
#[tokio::test]
async fn keys_with_control_characters_are_rejected() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("acme").await;
    srv.s3(&c, "PUT", "/ctl", &[], &[], vec![]).await;
    srv.s3(&c, "PUT", "/ctl/src", &[], &[], b"x".to_vec()).await;
    let bad = format!("/ctl/{}", path_encode("a\u{1}b"));
    let resp = srv.s3(&c, "PUT", &bad, &[], &[], b"x".to_vec()).await;
    assert_eq!(resp.status, 400, "{}", resp.text());
    assert_eq!(resp.error_code().as_deref(), Some("InvalidArgument"));
    let resp = srv
        .s3(&c, "POST", &bad, &[("uploads", "")], &[], vec![])
        .await;
    assert_eq!(resp.status, 400, "{}", resp.text());
    let resp = srv
        .s3(
            &c,
            "PUT",
            &bad,
            &[],
            &[("x-amz-copy-source", "/ctl/src")],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 400, "{}", resp.text());

    // Tab, LF and CR are valid XML and stay allowed.
    let ok = format!("/ctl/{}", path_encode("t\tn\nr\r"));
    let resp = srv.s3(&c, "PUT", &ok, &[], &[], b"x".to_vec()).await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let resp = srv
        .s3(&c, "GET", "/ctl", &[("list-type", "2")], &[], vec![])
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    assert_eq!(xml_values(&resp.text(), "Key").len(), 2);
}

/// A cluster out of space answers 507 (not retried by S3 clients), not a retryable 503.
#[tokio::test]
async fn writes_without_room_get_507() {
    let srv = Server::start_with(1, |c| c.storage.min_free_bytes = u64::MAX / 2).await;
    let (_, c) = srv.tenant("acme").await;
    srv.s3(&c, "PUT", "/bk1", &[], &[], vec![]).await;
    let resp = srv.s3(&c, "PUT", "/bk1/k", &[], &[], data(1000)).await;
    assert_eq!(resp.status, 507, "{}", resp.text());
    assert_eq!(
        xml_values(&resp.text(), "Code"),
        vec!["InsufficientStorage"]
    );
    let resp = srv
        .s3(&c, "POST", "/bk1/mp", &[("uploads", "")], &[], vec![])
        .await;
    assert_eq!(resp.status, 507, "{}", resp.text());

    // Counted for alerting.
    use tower::ServiceExt as _;
    let resp = srv
        .admin
        .clone()
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let text = String::from_utf8(
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(
        text.contains("suhms_storage_full_refusals_total 2"),
        "{text}"
    );
}
