//! Versioning, Object Lock, lifecycle and CORS over HTTP.

mod common;

use axum::{body::Body, http::Request};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use common::{Server, xml_values};
use md5::{Digest, Md5};

fn md5_b64(s: &str) -> String {
    B64.encode(Md5::digest(s.as_bytes()))
}

#[tokio::test]
async fn versioning_over_http() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/ver", &[], &[], vec![]).await;

    let cfg = "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>";
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/ver",
            &[("versioning", "")],
            &[],
            cfg.as_bytes().to_vec(),
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let resp = srv
        .s3(&c, "GET", "/ver", &[("versioning", "")], &[], vec![])
        .await;
    assert_eq!(xml_values(&resp.text(), "Status"), vec!["Enabled"]);

    let r1 = srv
        .s3(&c, "PUT", "/ver/doc", &[], &[], b"first".to_vec())
        .await;
    let r2 = srv
        .s3(&c, "PUT", "/ver/doc", &[], &[], b"second".to_vec())
        .await;
    let v1 = r1.header("x-amz-version-id").unwrap().to_owned();
    let v2 = r2.header("x-amz-version-id").unwrap().to_owned();
    assert_ne!(v1, v2);

    let resp = srv
        .s3(&c, "GET", "/ver/doc", &[("versionId", &v1)], &[], vec![])
        .await;
    assert_eq!(resp.body.as_ref(), b"first");
    assert_eq!(resp.header("x-amz-version-id"), Some(v1.as_str()));

    let resp = srv.s3(&c, "DELETE", "/ver/doc", &[], &[], vec![]).await;
    assert_eq!(resp.status, 204);
    assert_eq!(resp.header("x-amz-delete-marker"), Some("true"));
    let marker = resp.header("x-amz-version-id").unwrap().to_owned();
    assert_eq!(
        srv.s3(&c, "GET", "/ver/doc", &[], &[], vec![]).await.status,
        404
    );

    let resp = srv
        .s3(&c, "GET", "/ver", &[("versions", "")], &[], vec![])
        .await;
    let xml = resp.text();
    assert_eq!(xml.matches("<Version>").count(), 2, "{xml}");
    assert_eq!(xml.matches("<DeleteMarker>").count(), 1, "{xml}");

    // Removing the delete marker brings the key back.
    let resp = srv
        .s3(
            &c,
            "DELETE",
            "/ver/doc",
            &[("versionId", &marker)],
            &[],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 204);
    let resp = srv.s3(&c, "GET", "/ver/doc", &[], &[], vec![]).await;
    assert_eq!(resp.body.as_ref(), b"second");
}

#[tokio::test]
async fn object_lock_over_http() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/vault",
            &[],
            &[("x-amz-bucket-object-lock-enabled", "true")],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let cfg = "<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled>\
               <Rule><DefaultRetention><Mode>GOVERNANCE</Mode><Days>1</Days></DefaultRetention></Rule>\
               </ObjectLockConfiguration>";
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/vault",
            &[("object-lock", "")],
            &[("content-md5", &md5_b64(cfg))],
            cfg.as_bytes().to_vec(),
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let resp = srv
        .s3(&c, "GET", "/vault", &[("object-lock", "")], &[], vec![])
        .await;
    assert_eq!(xml_values(&resp.text(), "Mode"), vec!["GOVERNANCE"]);

    let resp = srv
        .s3(&c, "PUT", "/vault/backup.tar", &[], &[], b"data".to_vec())
        .await;
    let v = resp.header("x-amz-version-id").unwrap().to_owned();
    let resp = srv
        .s3(
            &c,
            "GET",
            "/vault/backup.tar",
            &[("retention", "")],
            &[],
            vec![],
        )
        .await;
    assert_eq!(
        xml_values(&resp.text(), "Mode"),
        vec!["GOVERNANCE"],
        "{}",
        resp.text()
    );

    let resp = srv
        .s3(
            &c,
            "DELETE",
            "/vault/backup.tar",
            &[("versionId", &v)],
            &[],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 403, "{}", resp.text());

    // Legal hold blocks even a governance bypass.
    let hold = "<LegalHold><Status>ON</Status></LegalHold>";
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/vault/backup.tar",
            &[("legal-hold", "")],
            &[("content-md5", &md5_b64(hold))],
            hold.as_bytes().to_vec(),
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let bypass = [("x-amz-bypass-governance-retention", "true")];
    let resp = srv
        .s3(
            &c,
            "DELETE",
            "/vault/backup.tar",
            &[("versionId", &v)],
            &bypass,
            vec![],
        )
        .await;
    assert_eq!(resp.status, 403);
    let off = "<LegalHold><Status>OFF</Status></LegalHold>";
    srv.s3(
        &c,
        "PUT",
        "/vault/backup.tar",
        &[("legal-hold", "")],
        &[("content-md5", &md5_b64(off))],
        off.as_bytes().to_vec(),
    )
    .await;
    let resp = srv
        .s3(
            &c,
            "DELETE",
            "/vault/backup.tar",
            &[("versionId", &v)],
            &bypass,
            vec![],
        )
        .await;
    assert_eq!(resp.status, 204, "{}", resp.text());

    // Versioning cannot be suspended on a locked bucket.
    let cfg = "<VersioningConfiguration><Status>Suspended</Status></VersioningConfiguration>";
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/vault",
            &[("versioning", "")],
            &[],
            cfg.as_bytes().to_vec(),
        )
        .await;
    assert_eq!(resp.status, 400);
}

#[tokio::test]
async fn lifecycle_configuration_round_trip() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/logs", &[], &[], vec![]).await;
    let resp = srv
        .s3(&c, "GET", "/logs", &[("lifecycle", "")], &[], vec![])
        .await;
    assert_eq!(
        resp.error_code().as_deref(),
        Some("NoSuchLifecycleConfiguration")
    );

    let cfg = "<LifecycleConfiguration><Rule><ID>old-logs</ID><Filter><Prefix>app/</Prefix></Filter>\
               <Status>Enabled</Status><Expiration><Days>30</Days></Expiration>\
               <NoncurrentVersionExpiration><NoncurrentDays>7</NoncurrentDays></NoncurrentVersionExpiration>\
               <AbortIncompleteMultipartUpload><DaysAfterInitiation>2</DaysAfterInitiation></AbortIncompleteMultipartUpload>\
               </Rule></LifecycleConfiguration>";
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/logs",
            &[("lifecycle", "")],
            &[("content-md5", &md5_b64(cfg))],
            cfg.as_bytes().to_vec(),
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let xml = srv
        .s3(&c, "GET", "/logs", &[("lifecycle", "")], &[], vec![])
        .await
        .text();
    assert_eq!(xml_values(&xml, "ID"), vec!["old-logs"], "{xml}");
    assert_eq!(xml_values(&xml, "Prefix"), vec!["app/"]);
    assert_eq!(xml_values(&xml, "Days"), vec!["30"]);
    assert_eq!(xml_values(&xml, "NoncurrentDays"), vec!["7"]);
    assert_eq!(xml_values(&xml, "DaysAfterInitiation"), vec!["2"]);

    assert_eq!(
        srv.s3(&c, "DELETE", "/logs", &[("lifecycle", "")], &[], vec![])
            .await
            .status,
        204
    );
    let resp = srv
        .s3(&c, "GET", "/logs", &[("lifecycle", "")], &[], vec![])
        .await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn cors_preflight_and_response_headers() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/web", &[], &[], vec![]).await;
    srv.s3(&c, "PUT", "/web/a.json", &[], &[], b"{}".to_vec())
        .await;
    let cfg = "<CORSConfiguration><CORSRule><AllowedOrigin>https://*.sorahost.net</AllowedOrigin>\
               <AllowedMethod>GET</AllowedMethod><AllowedMethod>PUT</AllowedMethod>\
               <AllowedHeader>*</AllowedHeader><ExposeHeader>ETag</ExposeHeader>\
               <MaxAgeSeconds>600</MaxAgeSeconds></CORSRule></CORSConfiguration>";
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/web",
            &[("cors", "")],
            &[("content-md5", &md5_b64(cfg))],
            cfg.as_bytes().to_vec(),
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let resp = srv
        .s3(&c, "GET", "/web", &[("cors", "")], &[], vec![])
        .await;
    assert_eq!(
        xml_values(&resp.text(), "AllowedOrigin"),
        vec!["https://*.sorahost.net"]
    );

    let preflight = |origin: &str, method: &str| {
        Request::builder()
            .method("OPTIONS")
            .uri("/web/a.json")
            .header("host", "localhost")
            .header("origin", origin)
            .header("access-control-request-method", method)
            .header("access-control-request-headers", "content-type")
            .body(Body::empty())
            .unwrap()
    };
    let resp = srv.send(preflight("https://app.sorahost.net", "PUT")).await;
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.header("access-control-allow-origin"),
        Some("https://app.sorahost.net")
    );
    assert_eq!(resp.header("access-control-max-age"), Some("600"));
    assert_eq!(
        srv.send(preflight("https://evil.example", "PUT"))
            .await
            .status,
        403
    );
    assert_eq!(
        srv.send(preflight("https://app.sorahost.net", "DELETE"))
            .await
            .status,
        403
    );

    let resp = srv
        .s3(
            &c,
            "GET",
            "/web/a.json",
            &[],
            &[("origin", "https://app.sorahost.net")],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.header("access-control-allow-origin"),
        Some("https://app.sorahost.net")
    );
    assert_eq!(resp.header("access-control-expose-headers"), Some("ETag"));

    assert_eq!(
        srv.s3(&c, "DELETE", "/web", &[("cors", "")], &[], vec![])
            .await
            .status,
        204
    );
    assert_eq!(
        srv.send(preflight("https://app.sorahost.net", "PUT"))
            .await
            .status,
        403
    );
}
