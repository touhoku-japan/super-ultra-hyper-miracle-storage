//! The Web consoles: customer login + S3 proxy, operator login + admin API extensions.

mod common;

use axum::{
    body::{Body, Bytes},
    http::{Request, StatusCode},
};
use common::{ADMIN_TOKEN, Creds, Server, xml_values};
use tower::ServiceExt;

struct Resp {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Bytes,
}

impl Resp {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
    /// `name=value` of the Set-Cookie header.
    fn cookie(&self) -> String {
        self.header("set-cookie")
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }
}

async fn call(router: &axum::Router, req: Request<Body>) -> Resp {
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    Resp {
        status,
        headers,
        body,
    }
}

fn req(method: &str, uri: &str, cookie: Option<&str>, body: impl Into<Body>) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-suhms-console", "1");
    if let Some(c) = cookie {
        b = b.header("cookie", c);
    }
    b.body(body.into()).unwrap()
}

fn json_req(uri: &str, cookie: Option<&str>, v: serde_json::Value) -> Request<Body> {
    let mut r = req("POST", uri, cookie, v.to_string());
    r.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    r
}

async fn login(srv: &Server, c: &Creds) -> String {
    let r = call(
        &srv.console,
        json_req(
            "/api/login",
            None,
            serde_json::json!({ "access_key_id": c.key, "secret_access_key": c.secret }),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let set = r.header("set-cookie");
    assert!(
        set.contains("HttpOnly") && set.contains("SameSite=Strict"),
        "{set}"
    );
    r.cookie()
}

#[tokio::test]
async fn customer_login_and_session() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;

    let r = call(
        &srv.console,
        req("GET", "/api/session", None, Body::empty()),
    )
    .await;
    assert_eq!(r.json()["logged_in"], false);
    assert_eq!(r.json()["mode"], "customer");

    let wrong = call(
        &srv.console,
        json_req(
            "/api/login",
            None,
            serde_json::json!({ "access_key_id": c.key, "secret_access_key": "nope" }),
        ),
    )
    .await;
    assert_eq!(wrong.status, 401);
    assert!(wrong.header("set-cookie").is_empty());

    let cookie = login(&srv, &c).await;
    let r = call(
        &srv.console,
        req("GET", "/api/session", Some(&cookie), Body::empty()),
    )
    .await;
    assert_eq!(r.json()["logged_in"], true);
    assert_eq!(r.json()["tenant"]["name"], "alice");
    assert_eq!(r.json()["access_key_id"], c.key.as_str());

    let r = call(
        &srv.console,
        req("POST", "/api/logout", Some(&cookie), Body::empty()),
    )
    .await;
    assert!(r.header("set-cookie").contains("Max-Age=0"));
}

#[tokio::test]
async fn proxy_runs_s3_requests_as_the_session_key() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    let cookie = login(&srv, &c).await;
    let ck = Some(cookie.as_str());

    assert_eq!(
        call(&srv.console, req("PUT", "/s3/photos", ck, Body::empty()))
            .await
            .status,
        200
    );
    let key = "dir/a b+c!'()*&日本.txt";
    let enc: String = key
        .split('/')
        .map(|s| {
            s.bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-_.~!'()*".contains(&b) {
                        char::from(b).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/");
    let mut put = req("PUT", &format!("/s3/photos/{enc}"), ck, "hello");
    put.headers_mut()
        .insert("content-length", "5".parse().unwrap());
    put.headers_mut()
        .insert("x-amz-meta-note", "hi".parse().unwrap());
    let r = call(&srv.console, put).await;
    assert_eq!(r.status, 200, "{}", r.text());

    // Downloads are attachments with a sandbox CSP, never inline content.
    let r = call(
        &srv.console,
        req("GET", &format!("/s3/photos/{enc}"), ck, Body::empty()),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), "hello");
    assert_eq!(r.header("x-amz-meta-note"), "hi");
    assert_eq!(r.header("content-disposition"), "attachment");
    assert!(r.header("content-security-policy").starts_with("sandbox"));
    assert_eq!(r.header("x-content-type-options"), "nosniff");

    let r = call(
        &srv.console,
        req(
            "GET",
            "/s3/photos/x?response-content-disposition=attachment%3B%20filename%3D%22n.txt%22",
            ck,
            Body::empty(),
        ),
    )
    .await;
    assert_eq!(r.status, 404); // no such key, but the query made it through signing

    let r = call(
        &srv.console,
        req(
            "GET",
            "/s3/photos?list-type=2&prefix=dir%2F",
            ck,
            Body::empty(),
        ),
    )
    .await;
    assert_eq!(r.status, 200);
    let listed = xml_values(&r.text(), "Key").remove(0);
    let listed = listed
        .replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    assert_eq!(listed, key);

    // ListBuckets at the root.
    let r = call(&srv.console, req("GET", "/s3/", ck, Body::empty())).await;
    assert_eq!(xml_values(&r.text(), "Name"), vec!["photos".to_owned()]);

    // The console's own listing adds the settings S3 has no call for.
    let r = call(&srv.console, req("GET", "/api/buckets", ck, Body::empty())).await;
    assert_eq!(r.json()[0]["name"], "photos");
    assert_eq!(r.json()[0]["public_read"], false);

    let r = call(&srv.console, req("GET", "/api/usage", ck, Body::empty())).await;
    assert_eq!(r.json()["objects"], 1);
    assert_eq!(r.json()["buckets"], 1);
}

#[tokio::test]
async fn proxy_supports_multipart_uploads() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    let cookie = login(&srv, &c).await;
    let ck = Some(cookie.as_str());
    call(&srv.console, req("PUT", "/s3/bkt", ck, Body::empty())).await;

    let r = call(
        &srv.console,
        req("POST", "/s3/bkt/big?uploads", ck, Body::empty()),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let upload = xml_values(&r.text(), "UploadId").remove(0);
    let part1 = vec![b'a'; 5 * 1024 * 1024];
    let mut etags = Vec::new();
    for (n, data) in [(1, part1.clone()), (2, b"tail".to_vec())] {
        let mut r = req(
            "PUT",
            &format!("/s3/bkt/big?partNumber={n}&uploadId={upload}"),
            ck,
            data.clone(),
        );
        r.headers_mut()
            .insert("content-length", data.len().to_string().parse().unwrap());
        let r = call(&srv.console, r).await;
        assert_eq!(r.status, 200, "{}", r.text());
        etags.push(r.header("etag").to_owned());
    }
    let body = format!(
        "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part>\
         <Part><PartNumber>2</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>",
        etags[0], etags[1]
    );
    let len = body.len();
    let mut complete = req("POST", &format!("/s3/bkt/big?uploadId={upload}"), ck, body);
    complete
        .headers_mut()
        .insert("content-length", len.to_string().parse().unwrap());
    let r = call(&srv.console, complete).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = call(&srv.console, req("HEAD", "/s3/bkt/big", ck, Body::empty())).await;
    assert_eq!(
        r.header("content-length"),
        (5 * 1024 * 1024 + 4).to_string()
    );
}

#[tokio::test]
async fn writes_need_the_csrf_header_and_a_session() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    let cookie = login(&srv, &c).await;

    let no_csrf = Request::put("/s3/photos")
        .header("cookie", &cookie)
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(&srv.console, no_csrf).await.status, 403);

    let no_login = call(&srv.console, req("GET", "/s3/", None, Body::empty())).await;
    assert_eq!(no_login.status, 401);

    let forged = call(
        &srv.console,
        req(
            "GET",
            "/s3/",
            Some("suhms_console=eyJzdWIiOiJ4In0.AAAA"),
            Body::empty(),
        ),
    )
    .await;
    assert_eq!(forged.status, 401);

    // Login itself is a write too (a cross-site form could otherwise log a victim in).
    let r = Request::post("/api/login")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "access_key_id": c.key, "secret_access_key": c.secret })
                .to_string(),
        ))
        .unwrap();
    assert_eq!(call(&srv.console, r).await.status, 403);
}

#[tokio::test]
async fn tenants_cannot_reach_each_other_through_the_console() {
    let srv = Server::start(1).await;
    let (_, alice) = srv.tenant("alice").await;
    let (_, bob) = srv.tenant("bob").await;
    let a = login(&srv, &alice).await;
    let b = login(&srv, &bob).await;
    call(
        &srv.console,
        req("PUT", "/s3/alices", Some(&a), Body::empty()),
    )
    .await;
    let mut put = req("PUT", "/s3/alices/secret", Some(&a), "x");
    put.headers_mut()
        .insert("content-length", "1".parse().unwrap());
    assert_eq!(call(&srv.console, put).await.status, 200);

    for (m, path) in [
        ("GET", "/s3/alices/secret"),
        ("GET", "/s3/alices?list-type=2"),
        ("DELETE", "/s3/alices/secret"),
    ] {
        let r = call(&srv.console, req(m, path, Some(&b), Body::empty())).await;
        assert_eq!(r.status, 403, "{m} {path}: {}", r.text());
    }
    let r = call(
        &srv.console,
        req("GET", "/api/buckets", Some(&b), Body::empty()),
    )
    .await;
    assert_eq!(r.json(), serde_json::json!([]));
    let r = call(
        &srv.console,
        json_req(
            "/api/presign",
            Some(&b),
            serde_json::json!({ "bucket": "alices", "key": "secret", "expires_secs": 60 }),
        ),
    )
    .await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn disabled_keys_and_suspended_tenants_lose_their_sessions() {
    let srv = Server::start(1).await;
    let (tenant, c) = srv.tenant("alice").await;
    let cookie = login(&srv, &c).await;
    let ck = Some(cookie.as_str());

    let (s, _) = srv
        .admin(
            "PATCH",
            &format!("/admin/v1/tenants/{tenant}"),
            Some(serde_json::json!({ "suspended": true })),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        call(&srv.console, req("GET", "/s3/", ck, Body::empty()))
            .await
            .status,
        403
    );
    // The login screen can say why.
    let r = call(&srv.console, req("GET", "/api/session", ck, Body::empty())).await;
    assert_eq!(r.json()["logged_in"], false);
    assert_eq!(r.json()["notice"], "このアカウントは停止されています");
    srv.admin(
        "PATCH",
        &format!("/admin/v1/tenants/{tenant}"),
        Some(serde_json::json!({ "suspended": false })),
    )
    .await;
    assert_eq!(
        call(&srv.console, req("GET", "/s3/", ck, Body::empty()))
            .await
            .status,
        200
    );

    let (s, _) = srv
        .admin(
            "PATCH",
            &format!("/admin/v1/keys/{}", c.key),
            Some(serde_json::json!({ "active": false })),
        )
        .await;
    assert_eq!(s, 204);
    assert_eq!(
        call(&srv.console, req("GET", "/s3/", ck, Body::empty()))
            .await
            .status,
        401
    );
    let r = call(&srv.console, req("GET", "/api/session", ck, Body::empty())).await;
    assert_eq!(r.json()["logged_in"], false);
    assert!(r.json().get("notice").is_none());
}

#[tokio::test]
async fn repeated_login_failures_are_throttled() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    for _ in 0..10 {
        let r = call(
            &srv.console,
            json_req(
                "/api/login",
                None,
                serde_json::json!({ "access_key_id": c.key, "secret_access_key": "wrong" }),
            ),
        )
        .await;
        assert_eq!(r.status, 401);
    }
    let r = call(
        &srv.console,
        json_req(
            "/api/login",
            None,
            serde_json::json!({ "access_key_id": c.key, "secret_access_key": c.secret }),
        ),
    )
    .await;
    assert_eq!(r.status, 429);
}

#[tokio::test]
async fn share_links_are_presigned_urls_for_the_s3_endpoint() {
    let srv = Server::start_with(1, |c| {
        c.console.s3_public_url = Some("http://localhost".into());
    })
    .await;
    let (_, c) = srv.tenant("alice").await;
    let cookie = login(&srv, &c).await;
    let ck = Some(cookie.as_str());
    call(&srv.console, req("PUT", "/s3/bkt", ck, Body::empty())).await;
    let mut put = req("PUT", "/s3/bkt/a%20file.txt", ck, "shared");
    put.headers_mut()
        .insert("content-length", "6".parse().unwrap());
    call(&srv.console, put).await;

    let r = call(
        &srv.console,
        json_req(
            "/api/presign",
            ck,
            serde_json::json!({ "bucket": "bkt", "key": "a file.txt", "expires_secs": 600 }),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let url = r.json()["url"].as_str().unwrap().to_owned();
    let path = url.strip_prefix("http://localhost").unwrap();
    let got = srv
        .send(
            Request::get(path)
                .header("host", "localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(got.status, 200, "{}", got.text());
    assert_eq!(got.text(), "shared");

    let r = call(
        &srv.console,
        json_req(
            "/api/presign",
            ck,
            serde_json::json!({ "bucket": "bkt", "key": "a file.txt", "expires_secs": 8 * 86400 }),
        ),
    )
    .await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn console_pages_have_security_headers() {
    let srv = Server::start(1).await;
    for path in ["/", "/buckets/x"] {
        let r = call(&srv.console, req("GET", path, None, Body::empty())).await;
        assert_eq!(r.status, 200);
        assert!(r.header("content-type").starts_with("text/html"));
        assert!(
            r.header("content-security-policy")
                .contains("frame-ancestors 'none'")
        );
    }
    let r = call(
        &srv.console,
        req("GET", "/api/nothing", None, Body::empty()),
    )
    .await;
    assert_eq!(r.status, 404);
}

// ---- operator console ----

async fn operator_login(srv: &Server) -> String {
    let r = call(
        &srv.admin,
        json_req(
            "/ui/api/login",
            None,
            serde_json::json!({ "token": ADMIN_TOKEN }),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.cookie()
}

#[tokio::test]
async fn operator_login_grants_the_admin_api() {
    let srv = Server::start(1).await;
    let r = call(
        &srv.admin,
        json_req(
            "/ui/api/login",
            None,
            serde_json::json!({ "token": "wrong" }),
        ),
    )
    .await;
    assert_eq!(r.status, 401);
    let r = call(
        &srv.admin,
        req("GET", "/admin/v1/overview", None, Body::empty()),
    )
    .await;
    assert_eq!(r.status, 401);

    let cookie = operator_login(&srv).await;
    let r = call(
        &srv.admin,
        req("GET", "/ui/api/session", Some(&cookie), Body::empty()),
    )
    .await;
    assert_eq!(r.json()["logged_in"], true);
    assert_eq!(r.json()["mode"], "operator");

    let r = call(
        &srv.admin,
        req("GET", "/admin/v1/overview", Some(&cookie), Body::empty()),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["disks"], 1);

    let r = call(
        &srv.admin,
        json_req(
            "/admin/v1/tenants",
            Some(&cookie),
            serde_json::json!({ "name": "carol" }),
        ),
    )
    .await;
    assert_eq!(r.status, 201);

    // Cookie writes need the CSRF header; Bearer clients (the portal) do not.
    let r = Request::post("/admin/v1/tenants")
        .header("cookie", &cookie)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"dave"}"#))
        .unwrap();
    assert_eq!(call(&srv.admin, r).await.status, 403);
    let (s, _) = srv
        .admin(
            "POST",
            "/admin/v1/tenants",
            Some(serde_json::json!({ "name": "dave" })),
        )
        .await;
    assert_eq!(s, 201);

    let r = call(&srv.admin, req("GET", "/ui/", None, Body::empty())).await;
    assert_eq!(r.status, 200);
    assert!(r.header("content-type").starts_with("text/html"));
}

#[tokio::test]
async fn customer_sessions_are_not_operator_sessions() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    let cookie = login(&srv, &c).await;
    let as_op = cookie.replacen("suhms_console=", "suhms_operator=", 1);
    let r = call(
        &srv.admin,
        req("GET", "/admin/v1/tenants", Some(&as_op), Body::empty()),
    )
    .await;
    assert_eq!(r.status, 401);
    let op = operator_login(&srv).await;
    let as_customer = op.replacen("suhms_operator=", "suhms_console=", 1);
    let r = call(
        &srv.console,
        req("GET", "/s3/", Some(&as_customer), Body::empty()),
    )
    .await;
    assert_eq!(r.status, 401);
}

#[tokio::test]
async fn disks_can_be_drained_and_removed_from_the_admin_api() {
    let srv = Server::start(2).await;
    let (_, c) = srv.tenant("alice").await;
    assert_eq!(
        srv.s3(&c, "PUT", "/bkt", &[], &[], vec![]).await.status,
        200
    );
    assert_eq!(
        srv.s3(&c, "PUT", "/bkt/k", &[], &[], b"data".to_vec())
            .await
            .status,
        200
    );

    let (s, disks) = srv.admin("GET", "/admin/v1/disks", None).await;
    assert_eq!(s, 200);
    let disks = disks.as_array().unwrap().clone();
    assert_eq!(disks.len(), 2);
    assert!(
        disks.iter().all(|d| d["up"] == true && d["copies"] == 1),
        "{disks:?}"
    );
    let id = disks[0]["id"].as_str().unwrap();

    let (s, body) = srv
        .admin("DELETE", &format!("/admin/v1/disks/{id}"), None)
        .await;
    assert_eq!(s, 409, "{body}");
    let (s, body) = srv
        .admin("POST", &format!("/admin/v1/disks/{id}/drain"), None)
        .await;
    assert_eq!(s, 200);
    assert_eq!(body["queued"], 1);
    assert!(
        body["warning"].is_string(),
        "2 copies cannot fit on 1 disk: {body}"
    );
    let (_, disks) = srv.admin("GET", "/admin/v1/disks", None).await;
    assert!(
        disks
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["draining"] == true)
    );
    let (s, _) = srv
        .admin("POST", &format!("/admin/v1/disks/{id}/undrain"), None)
        .await;
    assert_eq!(s, 200);

    let (s, _) = srv.admin("POST", "/admin/v1/nodes/nope/drain", None).await;
    assert_eq!(s, 404);
    let (s, _) = srv
        .admin("POST", "/admin/v1/disks/not-a-uuid/drain", None)
        .await;
    assert_eq!(s, 400);
}

#[tokio::test]
async fn maintenance_jobs_run_in_the_background() {
    let srv = Server::start(1).await;
    let (s, _) = srv.admin("POST", "/admin/v1/jobs/scrub", None).await;
    assert_eq!(s, 202);
    let (s, _) = srv.admin("POST", "/admin/v1/jobs/nope", None).await;
    assert_eq!(s, 404);
    let mut finished = None;
    for _ in 0..100 {
        let (_, jobs) = srv.admin("GET", "/admin/v1/jobs", None).await;
        let scrub = jobs
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["name"] == "scrub")
            .unwrap()
            .clone();
        if scrub["running"] == false && scrub["finished_at"].is_number() {
            finished = Some(scrub);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let scrub = finished.expect("scrub finishes");
    assert_eq!(scrub["result"]["checked"], 0);

    let (s, _) = srv
        .admin(
            "POST",
            "/admin/v1/jobs/gc",
            Some(serde_json::json!({ "grace_secs": 0 })),
        )
        .await;
    assert_eq!(s, 202);
    let (s, _) = srv
        .admin(
            "POST",
            "/admin/v1/jobs/rebalance",
            Some(serde_json::json!({ "bogus": 1 })),
        )
        .await;
    assert_eq!(s, 422);
}
