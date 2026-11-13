//! Commercial features: admin API, suspension, quotas, rate limits, public buckets, metering.

mod common;

use axum::{body::Body, http::Request};
use common::{Creds, Server, xml_values};
use serde_json::json;
use tower::ServiceExt;

fn creds(v: &serde_json::Value) -> Creds {
    Creds {
        key: v["access_key_id"].as_str().unwrap().to_owned(),
        secret: v["secret_access_key"].as_str().unwrap().to_owned(),
    }
}

#[tokio::test]
async fn admin_api_requires_its_token() {
    let srv = Server::start(1).await;
    let resp = srv
        .admin
        .clone()
        .oneshot(
            Request::get("/admin/v1/tenants")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let resp = srv
        .admin
        .clone()
        .oneshot(
            Request::get("/admin/v1/tenants")
                .header("authorization", "Bearer wrong")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    // Metrics are open (the admin listener is private).
    let resp = srv
        .admin
        .clone()
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn portal_flow_tenant_key_quota_suspend() {
    let srv = Server::start(1).await;
    let (st, t) = srv
        .admin(
            "POST",
            "/admin/v1/tenants",
            Some(json!({"name": "cust-001", "quota_bytes": 10})),
        )
        .await;
    assert_eq!(st, 201, "{t}");
    let id = t["id"].as_str().unwrap().to_owned();
    assert_eq!(t["quota_bytes"], 10);
    let (st, _) = srv
        .admin(
            "POST",
            "/admin/v1/tenants",
            Some(json!({"name": "cust-001"})),
        )
        .await;
    assert_eq!(st, 409);

    let (st, k) = srv
        .admin("POST", &format!("/admin/v1/tenants/{id}/keys"), None)
        .await;
    assert_eq!(st, 201);
    let c = creds(&k);

    assert_eq!(
        srv.s3(&c, "PUT", "/cust-bucket", &[], &[], vec![])
            .await
            .status,
        200
    );
    assert_eq!(
        srv.s3(&c, "PUT", "/cust-bucket/a", &[], &[], b"123456".to_vec())
            .await
            .status,
        200
    );
    let resp = srv
        .s3(&c, "PUT", "/cust-bucket/b", &[], &[], b"123456".to_vec())
        .await;
    assert_eq!(resp.status, 403);
    assert_eq!(resp.error_code().as_deref(), Some("QuotaExceeded"));

    let (st, t) = srv
        .admin(
            "PATCH",
            &format!("/admin/v1/tenants/{id}"),
            Some(json!({"quota_bytes": null})),
        )
        .await;
    assert_eq!(st, 200);
    assert!(t["quota_bytes"].is_null());
    assert_eq!(
        srv.s3(&c, "PUT", "/cust-bucket/b", &[], &[], b"123456".to_vec())
            .await
            .status,
        200
    );

    let (_, detail) = srv
        .admin("GET", &format!("/admin/v1/tenants/{id}"), None)
        .await;
    assert_eq!(
        (detail["objects"].as_u64(), detail["logical_bytes"].as_u64()),
        (Some(2), Some(12))
    );
    let (_, buckets) = srv
        .admin("GET", &format!("/admin/v1/tenants/{id}/buckets"), None)
        .await;
    assert_eq!(buckets[0]["name"], "cust-bucket");

    // Keys can be disabled; listing never shows secrets.
    let (_, keys) = srv
        .admin("GET", &format!("/admin/v1/tenants/{id}/keys"), None)
        .await;
    assert_eq!(keys.as_array().unwrap().len(), 1);
    assert!(keys[0].get("secret_access_key").is_none());

    // A fresh tenant suspended before use is refused immediately.
    let (_, t2) = srv
        .admin(
            "POST",
            "/admin/v1/tenants",
            Some(json!({"name": "cust-002"})),
        )
        .await;
    let id2 = t2["id"].as_str().unwrap().to_owned();
    let (_, k2) = srv
        .admin("POST", &format!("/admin/v1/tenants/{id2}/keys"), None)
        .await;
    srv.admin(
        "PATCH",
        &format!("/admin/v1/tenants/{id2}"),
        Some(json!({"suspended": true})),
    )
    .await;
    let resp = srv.s3(&creds(&k2), "GET", "/", &[], &[], vec![]).await;
    assert_eq!(resp.status, 403);
    assert!(resp.text().contains("suspended"), "{}", resp.text());
}

#[tokio::test]
async fn usage_is_metered_for_billing() {
    let srv = Server::start(1).await;
    let (tid, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/met", &[], &[], vec![]).await;
    srv.s3(&c, "PUT", "/met/k", &[], &[], vec![1u8; 1000]).await;
    srv.s3(&c, "GET", "/met/k", &[], &[], vec![]).await;
    srv.s3(&c, "GET", "/met/k", &[], &[("range", "bytes=0-99")], vec![])
        .await;
    srv.s3(&c, "HEAD", "/met/k", &[], &[], vec![]).await;
    srv.s3(&c, "GET", "/met", &[("list-type", "2")], &[], vec![])
        .await;
    srv.store.snapshot_storage().await.unwrap();

    let (st, u) = srv
        .admin(
            "GET",
            &format!("/admin/v1/tenants/{tid}/usage?from=0&to=99999999999999"),
            None,
        )
        .await;
    assert_eq!(st, 200, "{u}");
    assert_eq!(u["class_a_requests"], 3, "{u}"); // create bucket, put, list
    assert_eq!(u["class_b_requests"], 3, "{u}"); // get, ranged get, head
    assert_eq!(u["ingress_bytes"], 1000);
    assert_eq!(u["egress_bytes"], 1100);
    assert_eq!(u["logical_byte_hours"], 1000);

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
    assert!(text.contains("suhms_s3_requests_total 6"), "{text}");
    assert!(text.contains("suhms_disk_alive{"), "{text}");
}

#[tokio::test]
async fn rate_limit_answers_slow_down() {
    let srv = Server::start_with(1, |c| {
        c.limits.requests_per_second = 0.001;
        c.limits.burst = 2.0;
    })
    .await;
    let (_, c) = srv.tenant("alice").await;
    assert_eq!(srv.s3(&c, "GET", "/", &[], &[], vec![]).await.status, 200);
    assert_eq!(srv.s3(&c, "GET", "/", &[], &[], vec![]).await.status, 200);
    let resp = srv.s3(&c, "GET", "/", &[], &[], vec![]).await;
    assert_eq!(resp.status, 503);
    assert_eq!(resp.error_code().as_deref(), Some("SlowDown"));
}

#[tokio::test]
async fn public_buckets_need_server_permission_and_bucket_flag() {
    // Disabled by default.
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/site", &[], &[], vec![]).await;
    let (st, _) = srv
        .admin(
            "PATCH",
            "/admin/v1/buckets/site",
            Some(json!({"public_read": true})),
        )
        .await;
    assert_eq!(st, 403);

    let srv = Server::start_with(1, |c| c.s3.allow_public_buckets = true).await;
    let (tid, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/site", &[], &[], vec![]).await;
    srv.s3(
        &c,
        "PUT",
        "/site/index.html",
        &[],
        &[("content-type", "text/html")],
        b"<h1>hi</h1>".to_vec(),
    )
    .await;
    srv.s3(&c, "PUT", "/private", &[], &[], vec![]).await;
    srv.s3(&c, "PUT", "/private/secret", &[], &[], b"s".to_vec())
        .await;

    let anon = |path: &str, method: &str| {
        Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        srv.send(anon("/site/index.html", "GET")).await.status,
        403,
        "not yet public"
    );

    let (st, b) = srv
        .admin(
            "PATCH",
            "/admin/v1/buckets/site",
            Some(json!({"public_read": true})),
        )
        .await;
    assert_eq!((st.as_u16(), b["public_read"].as_bool()), (200, Some(true)));

    let resp = srv.send(anon("/site/index.html", "GET")).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body.as_ref(), b"<h1>hi</h1>");
    assert_eq!(srv.send(anon("/site/index.html", "HEAD")).await.status, 200);
    // Only reads, only that bucket.
    assert_eq!(srv.send(anon("/site/index.html", "PUT")).await.status, 403);
    assert_eq!(
        srv.send(anon("/site", "GET")).await.status,
        403,
        "no anonymous listing"
    );
    assert_eq!(srv.send(anon("/private/secret", "GET")).await.status, 403);

    // Anonymous downloads are billed to the bucket owner.
    srv.tenancy.meter.flush(&srv.store).await.unwrap();
    let (_, u) = srv
        .admin(
            "GET",
            &format!("/admin/v1/tenants/{tid}/usage?from=0&to=99999999999999"),
            None,
        )
        .await;
    assert_eq!(u["egress_bytes"], 11, "{u}");
    let _ = xml_values("", "x");
}
