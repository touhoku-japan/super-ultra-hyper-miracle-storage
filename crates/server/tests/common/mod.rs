//! Test harness: a real server stack (router + store on temp disks + SQLite) driven in-process
//! with SigV4-signed requests.

#![allow(dead_code)]

use std::{path::PathBuf, sync::Arc};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{Request, StatusCode},
};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use suhms_server::{
    admin,
    config::{Config, S3Config},
    crypto::SecretCipher,
};
use suhms_store::{Store, StoreConfig};
use tempfile::TempDir;
use tower::ServiceExt;

pub const REGION: &str = "jp-1";

#[derive(Clone)]
pub struct Creds {
    pub key: String,
    pub secret: String,
}

pub const ADMIN_TOKEN: &str = "admin-token-for-tests-0123456789abcdef";

pub struct Server {
    pub dir: TempDir,
    pub app: Router,
    pub admin: Router,
    pub tenancy: Arc<suhms_server::tenancy::Tenancy>,
    pub store: Arc<Store>,
    pub cipher: Arc<SecretCipher>,
    pub disks: Vec<PathBuf>,
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Bytes,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// The `<Code>` of an S3 error body.
    pub fn error_code(&self) -> Option<String> {
        xml_values(&self.text(), "Code").into_iter().next()
    }
}

/// All values of `<tag>...</tag>` in document order (good enough for test assertions).
pub fn xml_values(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(after[..j].to_owned());
        rest = &after[j + close.len()..];
    }
    out
}

impl Server {
    pub async fn start(disks: usize) -> Self {
        Self::start_with(disks, |_| {}).await
    }

    /// Starts a server after letting `configure` adjust the config.
    pub async fn start_with(disks: usize, configure: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let disk_paths: Vec<PathBuf> = (0..disks)
            .map(|i| {
                let p = dir.path().join(format!("disk{i}"));
                std::fs::create_dir(&p).unwrap();
                p
            })
            .collect();
        let mut config = Config {
            storage: StoreConfig {
                disks: disk_paths.clone(),
                metadata_url: format!("sqlite://{}", dir.path().join("meta.db").display()),
                default_replicas: u8::try_from(disks.min(3)).unwrap(),
                ..StoreConfig::default()
            },
            s3: S3Config {
                region: REGION.into(),
                ..S3Config::default()
            },
            ..Config::default()
        };
        configure(&mut config);
        let store = Arc::new(Store::open(config.storage.clone()).await.unwrap());
        let cipher = Arc::new(SecretCipher::from_hex(&SecretCipher::generate_hex()).unwrap());
        let tenancy = suhms_server::tenancy(Arc::clone(&store), &config);
        let app = suhms_server::app(
            Arc::clone(&store),
            Arc::clone(&tenancy),
            Arc::clone(&cipher),
            &config,
        )
        .unwrap();
        let admin = suhms_server::admin_api::router(
            Arc::clone(&store),
            Arc::clone(&tenancy),
            Arc::clone(&cipher),
            ADMIN_TOKEN,
            config.s3.allow_public_buckets,
        );
        Self {
            dir,
            app,
            admin,
            tenancy,
            store,
            cipher,
            disks: disk_paths,
        }
    }

    /// Creates a tenant with one access key.
    pub async fn tenant(&self, name: &str) -> (String, Creds) {
        let t = self.store.create_tenant(name).await.unwrap();
        let (key, secret) = admin::issue_key(self.store.metadata().as_ref(), &self.cipher, &t.id)
            .await
            .unwrap();
        (t.id, Creds { key, secret })
    }

    /// Calls the admin API with the test token; `body` is JSON.
    pub async fn admin(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {ADMIN_TOKEN}"));
        let body = match body {
            Some(v) => {
                req = req.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let resp = self
            .admin
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    pub async fn send(&self, req: Request<Body>) -> Resp {
        let resp = self.app.clone().oneshot(req).await.unwrap();
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

    /// Sends a signed request. `path` must already be percent-encoded; `query` values are raw.
    pub async fn s3(
        &self,
        creds: &Creds,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: impl Into<Bytes>,
    ) -> Resp {
        let req = signed(creds, method, path, query, headers, body.into());
        self.send(req).await
    }
}

fn uri_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-encodes a key for use in a path (keeps `/`).
pub fn path_encode(s: &str) -> String {
    s.split('/').map(uri_encode).collect::<Vec<_>>().join("/")
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).unwrap();
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

fn amz_date() -> (String, String) {
    let now = time::OffsetDateTime::now_utc();
    let date = format!(
        "{:04}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    );
    let stamp = format!(
        "{date}T{:02}{:02}{:02}Z",
        now.hour(),
        now.minute(),
        now.second()
    );
    (date, stamp)
}

/// Builds a request signed with AWS Signature Version 4 (header auth, signed payload).
pub fn signed(
    creds: &Creds,
    method: &str,
    path: &str,
    query: &[(&str, &str)],
    headers: &[(&str, &str)],
    body: Bytes,
) -> Request<Body> {
    let payload_hash = hex::encode(Sha256::digest(&body));
    signed_with_payload_hash(creds, method, path, query, headers, body, &payload_hash)
}

pub fn signed_with_payload_hash(
    creds: &Creds,
    method: &str,
    path: &str,
    query: &[(&str, &str)],
    headers: &[(&str, &str)],
    body: Bytes,
    payload_hash: &str,
) -> Request<Body> {
    let (date, stamp) = amz_date();
    let host = "localhost";

    let mut q: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k), uri_encode(v)))
        .collect();
    q.sort();
    let canonical_query = q
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    let mut signed_headers: Vec<(String, String)> = vec![
        ("host".into(), host.into()),
        ("x-amz-content-sha256".into(), payload_hash.into()),
        ("x-amz-date".into(), stamp.clone()),
    ];
    for (k, v) in headers {
        signed_headers.push((k.to_ascii_lowercase(), v.trim().to_owned()));
    }
    signed_headers.sort();
    let canonical_headers: String = signed_headers
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect();
    let signed_names = signed_headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{method}\n{path}\n{canonical_query}\n{canonical_headers}\n{signed_names}\n{payload_hash}"
    );
    let scope = format!("{date}/{REGION}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let k = hmac(format!("AWS4{}", creds.secret).as_bytes(), &date);
    let k = hmac(&k, REGION);
    let k = hmac(&k, "s3");
    let k = hmac(&k, "aws4_request");
    let signature = hex::encode(hmac(&k, &string_to_sign));

    let uri = if canonical_query.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{canonical_query}")
    };
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", host)
        .header("x-amz-date", &stamp)
        .header("x-amz-content-sha256", payload_hash)
        .header("content-length", body.len())
        .header(
            "authorization",
            format!(
                "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_names}, Signature={signature}",
                creds.key
            ),
        );
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    builder.body(Body::from(body)).unwrap()
}
