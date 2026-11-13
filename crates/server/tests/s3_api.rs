mod common;

use axum::{body::Body, http::Request};
use common::{Creds, Server, path_encode, signed_with_payload_hash, xml_values};
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
    srv.store
        .metadata()
        .set_access_key_active(&creds.key, false)
        .await
        .unwrap();
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
