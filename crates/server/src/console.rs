//! The customer Web console (`[console] listen`): the Svelte app, login with an access key, and
//! a proxy that runs the browser's S3 requests as that key.
//!
//! The browser never holds the secret. It logs in once (the secret is checked here and a signed
//! session cookie issued), then sends plain S3 requests to `/s3/...`; the proxy signs each one
//! with the session's key and hands it to the S3 service in-process. Everything S3 does —
//! tenant scoping, suspension, rate limits, metering, audit logging — therefore applies exactly
//! as for any SDK client.
//!
//! Endpoints (relative to the console root):
//! - `GET api/session`, `POST api/login`, `POST api/logout`
//! - `GET api/buckets` (with replica and public-read settings S3 has no call for), `GET api/usage`
//! - `POST api/presign` (share links, when `s3_public_url` is set)
//! - `/s3/<bucket>/<key>?<query>`: the S3 API

use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use suhms_store::{AccessKeyRecord, Store, TenantRecord, hour_of};
use tower::ServiceExt;

use crate::{
    config::Config,
    crypto::SecretCipher,
    session::{self, LoginLimiter, Sessions},
    sigv4::{self, Credentials},
    tenancy::Tenancy,
};

const COOKIE: &str = "suhms_console";
/// Longest share link (the SigV4 maximum).
const MAX_PRESIGN_SECS: u64 = 7 * 24 * 3600;

#[derive(Clone)]
struct Console(Arc<Inner>);

struct Inner {
    store: Arc<Store>,
    tenancy: Arc<Tenancy>,
    cipher: Arc<SecretCipher>,
    s3: Router,
    sessions: Sessions,
    logins: LoginLimiter,
    region: String,
    /// Host header of proxied requests: one that s3s parses as path-style.
    internal_host: String,
    public_url: Option<String>,
    trust_proxy: bool,
    storage_classes: Vec<(String, u8)>,
    default_replicas: u8,
}

struct ApiError(StatusCode, &'static str);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut r = (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response();
        session::security_headers(r.headers_mut());
        r
    }
}

impl From<suhms_store::StoreError> for ApiError {
    fn from(e: suhms_store::StoreError) -> Self {
        tracing::error!(error = %e, "console API error");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "内部エラーが発生しました",
        )
    }
}

const UNAUTHORIZED: ApiError = ApiError(StatusCode::UNAUTHORIZED, "ログインしてください");

/// The S3 app (from [`crate::app`]) wrapped in the console.
pub fn router(
    store: Arc<Store>,
    tenancy: Arc<Tenancy>,
    cipher: Arc<SecretCipher>,
    s3: Router,
    config: &Config,
) -> Router {
    let c = &config.console;
    let st = Console(Arc::new(Inner {
        sessions: Sessions::new(
            *cipher.session_key(),
            COOKIE,
            "/",
            Duration::from_secs(c.session_secs),
            c.secure_cookie,
        ),
        store,
        tenancy,
        cipher,
        s3,
        logins: LoginLimiter::default(),
        region: config.s3.region.clone(),
        internal_host: config
            .s3
            .domains
            .first()
            .cloned()
            .unwrap_or_else(|| "localhost".to_owned()),
        public_url: c.s3_public_url.clone(),
        trust_proxy: c.trust_proxy,
        storage_classes: config
            .storage
            .storage_classes
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
        default_replicas: config.storage.default_replicas,
    }));
    Router::new()
        .route("/api/session", get(session_info))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/buckets", get(buckets))
        .route("/api/usage", get(usage))
        .route("/api/presign", post(presign))
        .route("/s3", axum::routing::any(proxy))
        .route("/s3/", axum::routing::any(proxy))
        .route("/s3/{*rest}", axum::routing::any(proxy))
        .route("/api/{*rest}", axum::routing::any(|| async { not_found() }))
        .fallback(get(|req: Request| async move {
            crate::web::serve(req.uri().path())
        }))
        .with_state(st)
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "見つかりません")
}

impl Console {
    /// The logged-in key and its tenant, if the session is valid and both are still active.
    async fn caller(
        &self,
        headers: &HeaderMap,
    ) -> Result<(AccessKeyRecord, TenantRecord), ApiError> {
        let claims = self
            .0
            .sessions
            .from_headers(headers, sigv4::now_secs())
            .ok_or(UNAUTHORIZED)?;
        let dir = &self.0.tenancy.directory;
        let key = dir
            .key(&claims.sub)
            .await?
            .filter(|k| k.active)
            .ok_or(UNAUTHORIZED)?;
        let tenant = dir.tenant(&key.tenant_id).await?.ok_or(UNAUTHORIZED)?;
        if tenant.suspended {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "このアカウントは停止されています",
            ));
        }
        Ok((key, tenant))
    }

    fn secret(&self, key: &AccessKeyRecord) -> Result<String, ApiError> {
        self.0.cipher.decrypt(&key.id, &key.secret_enc).map_err(|e| {
            tracing::error!(access_key = key.id, error = %e, "cannot decrypt access key secret");
            ApiError(StatusCode::INTERNAL_SERVER_ERROR, "内部エラーが発生しました")
        })
    }
}

#[derive(Serialize)]
struct StorageClass {
    name: String,
    replicas: u8,
}

#[derive(Serialize)]
struct SessionInfo {
    mode: &'static str,
    logged_in: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    access_key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tenant: Option<TenantInfo>,
    /// Why a still-signed session is not logged in (a suspended tenant), for the login screen.
    #[serde(skip_serializing_if = "Option::is_none")]
    notice: Option<&'static str>,
    share_links: bool,
    storage_classes: Vec<StorageClass>,
    default_replicas: u8,
    max_share_secs: u64,
}

#[derive(Serialize)]
struct TenantInfo {
    name: String,
    quota_bytes: Option<u64>,
    max_buckets: Option<u64>,
}

async fn session_info(State(st): State<Console>, headers: HeaderMap) -> Response {
    let (caller, notice) = match st.caller(&headers).await {
        Ok(c) => (Some(c), None),
        Err(ApiError(StatusCode::FORBIDDEN, msg)) => (None, Some(msg)),
        Err(_) => (None, None),
    };
    let info = SessionInfo {
        mode: "customer",
        logged_in: caller.is_some(),
        access_key_id: caller.as_ref().map(|(k, _)| k.id.clone()),
        tenant: caller.map(|(_, t)| TenantInfo {
            name: t.name,
            quota_bytes: t.quota_bytes,
            max_buckets: t.max_buckets,
        }),
        notice,
        share_links: st.0.public_url.is_some(),
        storage_classes: st
            .0
            .storage_classes
            .iter()
            .map(|(name, replicas)| StorageClass {
                name: name.clone(),
                replicas: *replicas,
            })
            .collect(),
        default_replicas: st.0.default_replicas,
        max_share_secs: MAX_PRESIGN_SECS,
    };
    json_response(StatusCode::OK, &info)
}

fn json_response<T: Serialize>(status: StatusCode, v: &T) -> Response {
    let mut r = (status, Json(v)).into_response();
    let h = r.headers_mut();
    session::security_headers(h);
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

#[derive(Deserialize)]
struct Login {
    access_key_id: String,
    secret_access_key: String,
}

async fn login(State(st): State<Console>, req: Request) -> Response {
    match do_login(&st, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn do_login(st: &Console, req: Request) -> Result<Response, ApiError> {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let headers = req.headers().clone();
    if !session::csrf_ok(req.method(), &headers) {
        return Err(ApiError(StatusCode::FORBIDDEN, "不正なリクエストです"));
    }
    let body = axum::body::to_bytes(req.into_body(), 16 * 1024)
        .await
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "不正なリクエストです"))?;
    let login: Login = serde_json::from_slice(&body)
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "不正なリクエストです"))?;
    let ip = session::client_ip(&headers, peer, st.0.trust_proxy);
    let ip_key = format!("ip:{ip}");
    let id_key = format!("key:{}", login.access_key_id);
    let limiter_keys = [ip_key.as_str(), id_key.as_str()];
    if !st.0.logins.allowed(&limiter_keys) {
        tracing::warn!(target: "suhms::audit", ip, access_key = login.access_key_id, "console login throttled");
        return Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "ログインの失敗が多すぎます。しばらくしてからやり直してください",
        ));
    }
    let fail = |reason: &str| {
        st.0.logins.failed(&limiter_keys);
        tracing::info!(target: "suhms::audit", ip, access_key = login.access_key_id, reason, "console login failed");
        ApiError(
            StatusCode::UNAUTHORIZED,
            "アクセスキーまたはシークレットキーが正しくありません",
        )
    };
    let key =
        st.0.tenancy
            .directory
            .key(&login.access_key_id)
            .await
            .map_err(ApiError::from)?;
    let Some(key) = key.filter(|k| k.active) else {
        return Err(fail("unknown or disabled key"));
    };
    let secret = st.secret(&key)?;
    if !constant_time_eq(secret.as_bytes(), login.secret_access_key.as_bytes()) {
        return Err(fail("wrong secret"));
    }
    let tenant =
        st.0.tenancy
            .directory
            .tenant(&key.tenant_id)
            .await
            .map_err(ApiError::from)?;
    match tenant {
        Some(t) if !t.suspended => {}
        Some(_) => {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "このアカウントは停止されています",
            ));
        }
        None => return Err(fail("no tenant")),
    }
    tracing::info!(target: "suhms::audit", ip, access_key = key.id, tenant = key.tenant_id, "console login");
    let token = st.0.sessions.issue(&key.id, sigv4::now_secs());
    let mut r = json_response(StatusCode::OK, &serde_json::json!({ "ok": true }));
    r.headers_mut()
        .insert(header::SET_COOKIE, st.0.sessions.set_cookie(&token));
    Ok(r)
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn logout(State(st): State<Console>, headers: HeaderMap) -> Response {
    if let Some(c) = st.0.sessions.from_headers(&headers, sigv4::now_secs()) {
        tracing::info!(target: "suhms::audit", access_key = c.sub, "console logout");
    }
    let mut r = json_response(StatusCode::OK, &serde_json::json!({ "ok": true }));
    r.headers_mut()
        .insert(header::SET_COOKIE, st.0.sessions.clear_cookie());
    r
}

#[derive(Serialize)]
struct BucketInfo {
    name: String,
    created_at: i64,
    /// Replica count for objects without a storage class; `None` = cluster default.
    replicas: Option<u8>,
    public_read: bool,
}

async fn buckets(State(st): State<Console>, headers: HeaderMap) -> Result<Response, ApiError> {
    let (_, tenant) = st.caller(&headers).await?;
    let list: Vec<BucketInfo> =
        st.0.store
            .list_buckets(&tenant.id)
            .await?
            .into_iter()
            .map(|b| BucketInfo {
                name: b.name,
                created_at: b.created_at,
                replicas: b.replicas,
                public_read: b.public_read,
            })
            .collect();
    Ok(json_response(StatusCode::OK, &list))
}

#[derive(Serialize)]
struct UsageInfo {
    objects: u64,
    logical_bytes: u64,
    stored_bytes: u64,
    quota_bytes: Option<u64>,
    max_buckets: Option<u64>,
    buckets: usize,
    /// This calendar month (UTC) so far, from the meter (lags by up to the flush interval).
    month_start: i64,
    class_a_requests: u64,
    class_b_requests: u64,
    ingress_bytes: u64,
    egress_bytes: u64,
}

fn month_start_millis(now_secs: i64) -> i64 {
    let t = time::OffsetDateTime::from_unix_timestamp(now_secs)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let start = t
        .replace_day(1)
        .expect("day 1 exists")
        .replace_time(time::Time::MIDNIGHT);
    start.unix_timestamp() * 1000
}

async fn usage(State(st): State<Console>, headers: HeaderMap) -> Result<Response, ApiError> {
    let (_, tenant) = st.caller(&headers).await?;
    let meta = st.0.store.metadata();
    let stored = meta
        .storage_usage(Some(&tenant.id))
        .await?
        .into_iter()
        .next()
        .unwrap_or_default();
    let now = sigv4::now_secs();
    let from = month_start_millis(now);
    let hours = meta
        .usage(&tenant.id, hour_of(from), now * 1000 + 1)
        .await?;
    let buckets = st.0.store.list_buckets(&tenant.id).await?.len();
    Ok(json_response(
        StatusCode::OK,
        &UsageInfo {
            objects: stored.objects,
            logical_bytes: stored.logical_bytes,
            stored_bytes: stored.stored_bytes,
            quota_bytes: tenant.quota_bytes,
            max_buckets: tenant.max_buckets,
            buckets,
            month_start: from,
            class_a_requests: hours.iter().map(|h| h.class_a_requests).sum(),
            class_b_requests: hours.iter().map(|h| h.class_b_requests).sum(),
            ingress_bytes: hours.iter().map(|h| h.ingress_bytes).sum(),
            egress_bytes: hours.iter().map(|h| h.egress_bytes).sum(),
        },
    ))
}

#[derive(Deserialize)]
struct Presign {
    bucket: String,
    key: String,
    version_id: Option<String>,
    expires_secs: u64,
}

async fn presign(
    State(st): State<Console>,
    method: Method,
    headers: HeaderMap,
    Json(req): Json<Presign>,
) -> Result<Response, ApiError> {
    if !session::csrf_ok(&method, &headers) {
        return Err(ApiError(StatusCode::FORBIDDEN, "不正なリクエストです"));
    }
    let (key, tenant) = st.caller(&headers).await?;
    let Some(base) = st.0.public_url.as_deref() else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "共有リンクは無効になっています",
        ));
    };
    if req.expires_secs == 0 || req.expires_secs > MAX_PRESIGN_SECS {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "有効期限は 1 秒〜7 日です",
        ));
    }
    // Only for the caller's own buckets (the link would fail anyway, but say so now).
    match st.0.store.get_bucket(&tenant.id, &req.bucket).await {
        Ok(_) => {}
        Err(_) => return Err(not_found()),
    }
    let host = sigv4::url_host(base).ok_or(ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "s3_public_url が不正です",
    ))?;
    let secret = st.secret(&key)?;
    let creds = Credentials {
        access_key: &key.id,
        secret: &secret,
        region: &st.0.region,
    };
    let path = sigv4::path_encode(&format!("/{}/{}", req.bucket, req.key));
    let extra: Vec<(String, String)> = req
        .version_id
        .iter()
        .map(|v| ("versionId".to_owned(), v.clone()))
        .collect();
    let url = creds.presign_get(
        base,
        host,
        &path,
        &extra,
        req.expires_secs,
        sigv4::now_secs(),
    );
    tracing::info!(target: "suhms::audit", access_key = key.id, bucket = req.bucket, key = req.key, expires_secs = req.expires_secs, "console share link");
    Ok(json_response(
        StatusCode::OK,
        &serde_json::json!({ "url": url }),
    ))
}

/// Request headers passed on to S3 (besides `x-amz-*`); everything else, notably the cookie,
/// stays here.
const FORWARD: &[&str] = &[
    "content-type",
    "content-length",
    "content-md5",
    "content-disposition",
    "content-encoding",
    "content-language",
    "cache-control",
    "expires",
    "range",
    "if-match",
    "if-none-match",
    "if-modified-since",
    "if-unmodified-since",
];

/// `x-amz-*` headers the proxy sets itself (or that only belong to other auth schemes).
const OWN_AMZ: &[&str] = &[
    "x-amz-date",
    "x-amz-content-sha256",
    "x-amz-security-token",
    "x-amz-decoded-content-length",
];

/// Decodes a percent-encoded query string into raw pairs (`+` is a literal plus).
fn parse_query(query: &str) -> Option<Vec<(String, String)>> {
    query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            Some((
                percent_decode_str(k).decode_utf8().ok()?.into_owned(),
                percent_decode_str(v).decode_utf8().ok()?.into_owned(),
            ))
        })
        .collect()
}

async fn proxy(State(st): State<Console>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    if !session::csrf_ok(&parts.method, &parts.headers) {
        return ApiError(StatusCode::FORBIDDEN, "不正なリクエストです").into_response();
    }
    let (key, _) = match st.caller(&parts.headers).await {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let secret = match st.secret(&key) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    // Re-encode path and query canonically, so the request line is exactly what is signed.
    let raw_path = parts.uri.path().strip_prefix("/s3").unwrap_or("");
    let Ok(path) = percent_decode_str(raw_path).decode_utf8() else {
        return ApiError(StatusCode::BAD_REQUEST, "不正なパスです").into_response();
    };
    let path = if path.is_empty() { "/".into() } else { path };
    let encoded_path = sigv4::path_encode(&path);
    let Some(pairs) = parse_query(parts.uri.query().unwrap_or("")) else {
        return ApiError(StatusCode::BAD_REQUEST, "不正なクエリです").into_response();
    };
    let query = sigv4::canonical_query(&pairs);

    let now = sigv4::now_secs();
    let (_, stamp) = sigv4::amz_date(now);
    let mut forward = HeaderMap::new();
    let mut signed: Vec<(String, String)> = vec![
        ("host".into(), st.0.internal_host.clone()),
        ("x-amz-date".into(), stamp.clone()),
        ("x-amz-content-sha256".into(), "UNSIGNED-PAYLOAD".into()),
    ];
    for (name, value) in &parts.headers {
        let n = name.as_str();
        if n.starts_with("x-amz-") && !OWN_AMZ.contains(&n) {
            let Ok(v) = value.to_str() else {
                return ApiError(StatusCode::BAD_REQUEST, "不正なヘッダです").into_response();
            };
            signed.push((n.to_owned(), v.to_owned()));
            forward.append(name.clone(), value.clone());
        } else if FORWARD.contains(&n) {
            forward.append(name.clone(), value.clone());
        }
    }
    let creds = Credentials {
        access_key: &key.id,
        secret: &secret,
        region: &st.0.region,
    };
    let auth = creds.authorization(
        parts.method.as_str(),
        &encoded_path,
        &query,
        &signed,
        "UNSIGNED-PAYLOAD",
        now,
    );
    let uri = if query.is_empty() {
        encoded_path
    } else {
        format!("{encoded_path}?{query}")
    };
    let mut builder = Request::builder().method(parts.method.clone()).uri(uri);
    let Some(h) = builder.headers_mut() else {
        return ApiError(StatusCode::BAD_REQUEST, "不正なリクエストです").into_response();
    };
    *h = forward;
    for (k, v) in [
        ("host", st.0.internal_host.as_str()),
        ("x-amz-date", stamp.as_str()),
        ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
        ("authorization", auth.as_str()),
    ] {
        match HeaderValue::from_str(v) {
            Ok(v) => {
                h.insert(HeaderName::from_static(k), v);
            }
            Err(_) => {
                return ApiError(StatusCode::BAD_REQUEST, "不正なリクエストです").into_response();
            }
        }
    }
    let Ok(inner) = builder.body(body) else {
        return ApiError(StatusCode::BAD_REQUEST, "不正なリクエストです").into_response();
    };
    let mut resp = match st.0.s3.clone().oneshot(inner).await {
        Ok(r) => r,
        Err(e) => match e {},
    };
    harden_s3_response(resp.headers_mut());
    resp.map(Body::new)
}

/// Responses carry customer content on the console's origin: never let it render here. It is
/// always a download (a `response-content-disposition` asking for an attachment keeps its file
/// name), and a sandbox CSP stops scripts even if a browser ignored that.
fn harden_s3_response(h: &mut HeaderMap) {
    let keep = h
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.trim_start()
                .to_ascii_lowercase()
                .starts_with("attachment")
        });
    if !keep {
        h.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }
    h.insert(
        "content-security-policy",
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    h.remove(header::SET_COOKIE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_decoded() {
        assert_eq!(
            parse_query("list-type=2&prefix=a%20b%2F&delete").unwrap(),
            vec![
                ("list-type".into(), "2".into()),
                ("prefix".into(), "a b/".into()),
                ("delete".into(), String::new()),
            ]
        );
        assert_eq!(parse_query("").unwrap(), vec![]);
        assert!(parse_query("a=%FF").is_none());
    }

    #[test]
    fn month_start_is_utc_midnight_of_day_one() {
        // 2026-11-15T12:34:56Z -> 2026-11-01T00:00:00Z
        assert_eq!(month_start_millis(1_794_746_096), 1_793_491_200_000);
    }

    #[test]
    fn customer_content_is_never_inline() {
        let mut h = HeaderMap::new();
        h.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("inline"),
        );
        harden_s3_response(&mut h);
        assert_eq!(h[header::CONTENT_DISPOSITION], "attachment");
        let mut h = HeaderMap::new();
        h.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment; filename=\"a.txt\""),
        );
        harden_s3_response(&mut h);
        assert_eq!(
            h[header::CONTENT_DISPOSITION],
            "attachment; filename=\"a.txt\""
        );
        assert!(
            h["content-security-policy"]
                .to_str()
                .unwrap()
                .starts_with("sandbox")
        );
    }
}
