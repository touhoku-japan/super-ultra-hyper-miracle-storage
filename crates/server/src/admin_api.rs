//! Administration API for the SoraHost portal and the operator console: tenants, access keys,
//! quotas, suspension, bucket settings, usage for billing, disks, nodes and maintenance jobs.
//! JSON over HTTP on `[admin] listen`, with a bearer token (or, for the operator console at
//! `/ui/`, a session cookie obtained with that token). `/metrics` (Prometheus) needs no token;
//! keep the listener private.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, patch, post},
};
use serde::{Deserialize, Serialize};
use suhms_store::{
    BucketRecord, DiskRecord, RemoveDiskOutcome, Store, StoreError, TenantRecord, TenantUpdate,
    UsageRow, disk::DiskId,
};

use crate::{
    admin::{issue_key, now_millis},
    config::Config,
    console::constant_time_eq,
    crypto::{SecretCipher, derive_key},
    jobs::{JobParams, Jobs, StartError},
    session::{self, LoginLimiter, Sessions},
    tenancy::Tenancy,
};

const OPERATOR: &str = "operator";

#[derive(Clone)]
struct Admin {
    store: Arc<Store>,
    tenancy: Arc<Tenancy>,
    cipher: Arc<SecretCipher>,
    token: Arc<str>,
    allow_public: bool,
    sessions: Arc<Sessions>,
    logins: Arc<LoginLimiter>,
    jobs: Arc<Jobs>,
    /// The most copies any storage class (or the default) asks for.
    max_wanted_replicas: u8,
}

#[derive(Debug)]
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        let status = match &e {
            StoreError::NoSuchBucket(_) | StoreError::NoSuchTenant(_) => StatusCode::NOT_FOUND,
            StoreError::TenantAlreadyExists(_) | StoreError::BucketAlreadyExists(_) => {
                StatusCode::CONFLICT
            }
            StoreError::InvalidArgument(_)
            | StoreError::InvalidReplicaCount { .. }
            | StoreError::InvalidBucketName(_) => StatusCode::BAD_REQUEST,
            _ => {
                tracing::error!(error = %e, "admin API error");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        Self(status, e.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!(error = %e, "admin API error");
        Self(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

fn not_found(what: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("{what} not found"))
}

async fn authenticate(State(st): State<Admin>, req: Request, next: Next) -> Response {
    let bearer = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| constant_time_eq(t.as_bytes(), st.token.as_bytes()));
    if !bearer {
        let session = st
            .sessions
            .from_headers(req.headers(), now_millis() / 1000)
            .is_some_and(|c| c.sub == OPERATOR);
        if !session {
            return ApiError(StatusCode::UNAUTHORIZED, "invalid admin token".into())
                .into_response();
        }
        if !session::csrf_ok(req.method(), req.headers()) {
            return ApiError(StatusCode::FORBIDDEN, "missing CSRF header".into()).into_response();
        }
        tracing::info!(target: "suhms::audit", method = %req.method(), path = req.uri().path(), "operator console request");
    }
    let mut resp = next.run(req).await;
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    resp
}

#[derive(Serialize)]
struct Tenant {
    id: String,
    name: String,
    created_at: i64,
    suspended: bool,
    quota_bytes: Option<u64>,
    max_buckets: Option<u64>,
}

impl From<TenantRecord> for Tenant {
    fn from(t: TenantRecord) -> Self {
        Self {
            id: t.id,
            name: t.name,
            created_at: t.created_at,
            suspended: t.suspended,
            quota_bytes: t.quota_bytes,
            max_buckets: t.max_buckets,
        }
    }
}

#[derive(Serialize)]
struct TenantDetail {
    #[serde(flatten)]
    tenant: Tenant,
    objects: u64,
    logical_bytes: u64,
    stored_bytes: u64,
}

#[derive(Deserialize)]
struct CreateTenant {
    name: String,
    quota_bytes: Option<u64>,
    max_buckets: Option<u64>,
}

/// Absent fields are left unchanged; `null` clears a limit.
#[derive(Deserialize)]
struct PatchTenant {
    suspended: Option<bool>,
    #[serde(default, with = "double_option")]
    quota_bytes: Option<Option<u64>>,
    #[serde(default, with = "double_option")]
    max_buckets: Option<Option<u64>>,
}

mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(d).map(Some)
    }
}

async fn create_tenant(
    State(st): State<Admin>,
    Json(req): Json<CreateTenant>,
) -> Result<(StatusCode, Json<Tenant>), ApiError> {
    let t = st.store.create_tenant(&req.name).await?;
    if req.quota_bytes.is_some() || req.max_buckets.is_some() {
        st.store
            .metadata()
            .update_tenant(
                &t.id,
                &TenantUpdate {
                    quota_bytes: Some(req.quota_bytes),
                    max_buckets: Some(req.max_buckets),
                    ..TenantUpdate::default()
                },
            )
            .await?;
    }
    let t = st
        .store
        .metadata()
        .get_tenant(&t.id)
        .await?
        .ok_or_else(|| not_found("tenant"))?;
    Ok((StatusCode::CREATED, Json(t.into())))
}

async fn list_tenants(State(st): State<Admin>) -> ApiResult<Vec<Tenant>> {
    Ok(Json(
        st.store
            .metadata()
            .list_tenants()
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

async fn get_tenant(State(st): State<Admin>, Path(id): Path<String>) -> ApiResult<TenantDetail> {
    let t = st
        .store
        .metadata()
        .get_tenant(&id)
        .await?
        .ok_or_else(|| not_found("tenant"))?;
    let usage = st
        .store
        .metadata()
        .storage_usage(Some(&id))
        .await?
        .into_iter()
        .next()
        .unwrap_or_default();
    Ok(Json(TenantDetail {
        tenant: t.into(),
        objects: usage.objects,
        logical_bytes: usage.logical_bytes,
        stored_bytes: usage.stored_bytes,
    }))
}

async fn patch_tenant(
    State(st): State<Admin>,
    Path(id): Path<String>,
    Json(req): Json<PatchTenant>,
) -> ApiResult<Tenant> {
    let update = TenantUpdate {
        suspended: req.suspended,
        quota_bytes: req.quota_bytes,
        max_buckets: req.max_buckets,
    };
    if !st.store.metadata().update_tenant(&id, &update).await? {
        return Err(not_found("tenant"));
    }
    st.tenancy.directory.invalidate_tenant(&id).await;
    let t = st
        .store
        .metadata()
        .get_tenant(&id)
        .await?
        .ok_or_else(|| not_found("tenant"))?;
    Ok(Json(t.into()))
}

#[derive(Serialize)]
struct NewKey {
    access_key_id: String,
    /// Shown only in this response.
    secret_access_key: String,
}

#[derive(Serialize)]
struct Key {
    access_key_id: String,
    active: bool,
    created_at: i64,
}

async fn create_key(
    State(st): State<Admin>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<NewKey>), ApiError> {
    st.store
        .metadata()
        .get_tenant(&id)
        .await?
        .ok_or_else(|| not_found("tenant"))?;
    let (access_key_id, secret_access_key) =
        issue_key(st.store.metadata().as_ref(), &st.cipher, &id).await?;
    Ok((
        StatusCode::CREATED,
        Json(NewKey {
            access_key_id,
            secret_access_key,
        }),
    ))
}

async fn list_keys(State(st): State<Admin>, Path(id): Path<String>) -> ApiResult<Vec<Key>> {
    Ok(Json(
        st.store
            .metadata()
            .list_access_keys(&id)
            .await?
            .into_iter()
            .map(|k| Key {
                access_key_id: k.id,
                active: k.active,
                created_at: k.created_at,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct PatchKey {
    active: bool,
}

async fn patch_key(
    State(st): State<Admin>,
    Path(key): Path<String>,
    Json(req): Json<PatchKey>,
) -> Result<StatusCode, ApiError> {
    if !st
        .store
        .metadata()
        .set_access_key_active(&key, req.active)
        .await?
    {
        return Err(not_found("access key"));
    }
    st.tenancy.directory.invalidate_key(&key).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct Bucket {
    name: String,
    replicas: Option<u8>,
    public_read: bool,
    created_at: i64,
}

impl From<BucketRecord> for Bucket {
    fn from(b: BucketRecord) -> Self {
        Self {
            name: b.name,
            replicas: b.replicas,
            public_read: b.public_read,
            created_at: b.created_at,
        }
    }
}

async fn list_buckets(State(st): State<Admin>, Path(id): Path<String>) -> ApiResult<Vec<Bucket>> {
    Ok(Json(
        st.store
            .list_buckets(&id)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[derive(Deserialize)]
struct PatchBucket {
    /// A replica count, or `null` for the cluster default.
    #[serde(default, with = "double_option")]
    replicas: Option<Option<u8>>,
    public_read: Option<bool>,
}

async fn patch_bucket(
    State(st): State<Admin>,
    Path(name): Path<String>,
    Json(req): Json<PatchBucket>,
) -> ApiResult<Bucket> {
    if let Some(replicas) = req.replicas {
        st.store.set_bucket_replicas(&name, replicas).await?;
    }
    if let Some(public) = req.public_read {
        if public && !st.allow_public {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "public buckets are disabled ([s3] allow_public_buckets)".into(),
            ));
        }
        st.store.set_bucket_public(&name, public).await?;
    }
    let b = st
        .store
        .metadata()
        .get_bucket(&name)
        .await?
        .ok_or_else(|| not_found("bucket"))?;
    Ok(Json(b.into()))
}

#[derive(Deserialize)]
struct UsageQuery {
    /// Unix ms (inclusive), rounded down to the hour.
    from: i64,
    /// Unix ms (exclusive).
    to: i64,
}

#[derive(Serialize)]
struct Usage {
    hours: Vec<UsageRow>,
    /// Sum of hourly snapshots of logical bytes: divide by the hours in a month for GB-months.
    logical_byte_hours: u64,
    class_a_requests: u64,
    class_b_requests: u64,
    ingress_bytes: u64,
    egress_bytes: u64,
}

async fn usage(
    State(st): State<Admin>,
    Path(id): Path<String>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Usage> {
    // Make sure the latest counters are in the meter.
    if let Err(error) = st.tenancy.meter.flush(&st.store).await {
        tracing::warn!(%error, "usage flush failed");
    }
    let hours = st
        .store
        .metadata()
        .usage(&id, suhms_store::hour_of(q.from), q.to)
        .await?;
    Ok(Json(Usage {
        logical_byte_hours: hours.iter().map(|h| h.logical_bytes).sum(),
        class_a_requests: hours.iter().map(|h| h.class_a_requests).sum(),
        class_b_requests: hours.iter().map(|h| h.class_b_requests).sum(),
        ingress_bytes: hours.iter().map(|h| h.ingress_bytes).sum(),
        egress_bytes: hours.iter().map(|h| h.egress_bytes).sum(),
        hours,
    }))
}

#[derive(Serialize)]
struct DiskStatus {
    id: String,
    node_id: String,
    alive: bool,
    free_bytes: u64,
}

async fn cluster(State(st): State<Admin>) -> ApiResult<Vec<DiskStatus>> {
    st.store.refresh_cluster().await;
    Ok(Json(
        st.store
            .cluster_disks()
            .await?
            .iter()
            .map(|d| DiskStatus {
                id: d.id.to_string(),
                node_id: d.node_id.clone(),
                alive: d.alive,
                free_bytes: d.free,
            })
            .collect(),
    ))
}

async fn metrics(State(st): State<Admin>) -> impl IntoResponse {
    (
        [("content-type", "text/plain; version=0.0.4")],
        st.tenancy.metrics(&st.store).await,
    )
}

pub fn router(
    store: Arc<Store>,
    tenancy: Arc<Tenancy>,
    cipher: Arc<SecretCipher>,
    token: &str,
    config: &Config,
) -> Router {
    let m = &config.maintenance;
    let st = Admin {
        jobs: Arc::new(Jobs::new(
            Arc::clone(&store),
            Duration::from_secs(m.gc_grace_secs),
            m.checksum_backfill_mib_per_sec,
        )),
        store,
        tenancy,
        cipher,
        token: Arc::from(token),
        allow_public: config.s3.allow_public_buckets,
        sessions: Arc::new(Sessions::new(
            derive_key(token.as_bytes(), "suhms operator session v1"),
            "suhms_operator",
            "/",
            Duration::from_secs(config.admin.ui_session_secs),
            config.admin.ui_secure_cookie,
        )),
        logins: Arc::new(LoginLimiter::default()),
        max_wanted_replicas: config
            .storage
            .storage_classes
            .values()
            .copied()
            .chain([config.storage.default_replicas])
            .max()
            .unwrap_or(1),
    };
    let api = Router::new()
        .route("/admin/v1/tenants", post(create_tenant).get(list_tenants))
        .route(
            "/admin/v1/tenants/{id}",
            get(get_tenant).patch(patch_tenant),
        )
        .route(
            "/admin/v1/tenants/{id}/keys",
            post(create_key).get(list_keys),
        )
        .route("/admin/v1/tenants/{id}/buckets", get(list_buckets))
        .route("/admin/v1/tenants/{id}/usage", get(usage))
        .route("/admin/v1/keys/{key}", patch(patch_key))
        .route("/admin/v1/buckets/{name}", patch(patch_bucket))
        .route("/admin/v1/cluster", get(cluster))
        .route("/admin/v1/overview", get(overview))
        .route("/admin/v1/storage", get(storage))
        .route("/admin/v1/disks", get(list_disks))
        .route("/admin/v1/disks/{id}", axum::routing::delete(remove_disk))
        .route("/admin/v1/disks/{id}/drain", post(drain_disk))
        .route("/admin/v1/disks/{id}/undrain", post(undrain_disk))
        .route("/admin/v1/nodes/{id}", axum::routing::delete(remove_node))
        .route("/admin/v1/nodes/{id}/drain", post(drain_node))
        .route("/admin/v1/nodes/{id}/undrain", post(undrain_node))
        .route("/admin/v1/jobs", get(list_jobs))
        .route("/admin/v1/jobs/{name}", post(start_job))
        .layer(middleware::from_fn_with_state(st.clone(), authenticate));
    let mut r = Router::new().route("/metrics", get(metrics)).merge(api);
    if config.admin.ui {
        r = r
            .route("/ui", get(|| async { Redirect::permanent("/ui/") }))
            .route("/ui/api/session", get(ui_session))
            .route("/ui/api/login", post(ui_login))
            .route("/ui/api/logout", post(ui_logout))
            .route(
                "/ui/api/{*rest}",
                axum::routing::any(|| async { StatusCode::NOT_FOUND }),
            )
            .route("/ui/", get(|| async { crate::web::serve("") }))
            .route(
                "/ui/{*path}",
                get(|Path(path): Path<String>| async move { crate::web::serve(&path) }),
            );
    }
    r.with_state(st)
}

// ---- operator console sessions ----

fn ui_json<T: Serialize>(status: StatusCode, v: &T) -> Response {
    let mut r = (status, Json(v)).into_response();
    session::security_headers(r.headers_mut());
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    r
}

async fn ui_session(State(st): State<Admin>, headers: HeaderMap) -> Response {
    let logged_in = st
        .sessions
        .from_headers(&headers, now_millis() / 1000)
        .is_some_and(|c| c.sub == OPERATOR);
    ui_json(
        StatusCode::OK,
        &serde_json::json!({
            "mode": "operator",
            "logged_in": logged_in,
            "node_id": st.store.node_id(),
            "allow_public_buckets": st.allow_public,
        }),
    )
}

#[derive(Deserialize)]
struct UiLogin {
    token: String,
}

async fn ui_login(State(st): State<Admin>, req: Request) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let headers = req.headers().clone();
    let bad = || {
        ui_json(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({ "error": "不正なリクエストです" }),
        )
    };
    if !session::csrf_ok(req.method(), &headers) {
        return bad();
    }
    let Ok(body) = axum::body::to_bytes(req.into_body(), 16 * 1024).await else {
        return bad();
    };
    let Ok(login) = serde_json::from_slice::<UiLogin>(&body) else {
        return bad();
    };
    let ip = session::client_ip(&headers, peer, false);
    let ip_key = format!("ip:{ip}");
    if !st.logins.allowed(&[&ip_key]) {
        tracing::warn!(target: "suhms::audit", ip, "operator console login throttled");
        return ui_json(
            StatusCode::TOO_MANY_REQUESTS,
            &serde_json::json!({ "error": "ログインの失敗が多すぎます。しばらくしてからやり直してください" }),
        );
    }
    if !constant_time_eq(login.token.trim().as_bytes(), st.token.as_bytes()) {
        st.logins.failed(&[&ip_key]);
        tracing::warn!(target: "suhms::audit", ip, "operator console login failed");
        return ui_json(
            StatusCode::UNAUTHORIZED,
            &serde_json::json!({ "error": "管理トークンが正しくありません" }),
        );
    }
    tracing::info!(target: "suhms::audit", ip, "operator console login");
    let token = st.sessions.issue(OPERATOR, now_millis() / 1000);
    let mut r = ui_json(StatusCode::OK, &serde_json::json!({ "ok": true }));
    r.headers_mut()
        .insert(header::SET_COOKIE, st.sessions.set_cookie(&token));
    r
}

async fn ui_logout(State(st): State<Admin>) -> Response {
    let mut r = ui_json(StatusCode::OK, &serde_json::json!({ "ok": true }));
    r.headers_mut()
        .insert(header::SET_COOKIE, st.sessions.clear_cookie());
    r
}

// ---- cluster ----

/// A disk counts as up when its node heartbeated recently (as the server judges it).
fn is_up(d: &DiskRecord) -> bool {
    d.online && d.heartbeat_at >= now_millis() - 30_000
}

#[derive(Serialize)]
struct DiskInfo {
    id: String,
    node_id: String,
    path: String,
    up: bool,
    draining: bool,
    free_bytes: u64,
    heartbeat_at: i64,
    down_since: Option<i64>,
    copies: u64,
}

async fn list_disks(State(st): State<Admin>) -> ApiResult<Vec<DiskInfo>> {
    let meta = st.store.metadata();
    let mut out = Vec::new();
    for d in meta.list_disks().await? {
        out.push(DiskInfo {
            id: d.id.to_string(),
            up: is_up(&d),
            copies: meta.count_disk_replicas(d.id).await?,
            node_id: d.node_id,
            path: d.path,
            draining: d.draining,
            free_bytes: d.free_bytes,
            heartbeat_at: d.heartbeat_at,
            down_since: d.down_since,
        });
    }
    Ok(Json(out))
}

fn parse_disk(s: &str) -> Result<DiskId, ApiError> {
    DiskId::try_parse(s)
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, format!("{s} is not a disk id")))
}

#[derive(Serialize)]
struct DrainResult {
    disks: usize,
    queued: u64,
    /// Set when the disks left cannot hold every object's copies.
    warning: Option<String>,
}

async fn set_draining(st: &Admin, disks: &[DiskId], draining: bool) -> ApiResult<DrainResult> {
    let meta = st.store.metadata();
    let mut queued = 0;
    for disk in disks {
        if !meta.set_disk_draining(*disk, draining).await? {
            return Err(not_found("disk"));
        }
        queued += meta.enqueue_disk_repairs(*disk).await?;
    }
    st.store.refresh_cluster().await;
    tracing::info!(target: "suhms::audit", ?disks, draining, "disk draining changed");
    let warning = if draining {
        capacity_warning(&meta.list_disks().await?, st.max_wanted_replicas)
    } else {
        None
    };
    Ok(Json(DrainResult {
        disks: disks.len(),
        queued,
        warning,
    }))
}

fn capacity_warning(disks: &[DiskRecord], needed: u8) -> Option<String> {
    use std::collections::HashSet;
    let all_nodes: HashSet<&str> = disks.iter().map(|d| d.node_id.as_str()).collect();
    let active: Vec<&DiskRecord> = disks.iter().filter(|d| !d.draining).collect();
    let active_nodes: HashSet<&str> = active.iter().map(|d| d.node_id.as_str()).collect();
    let (room, unit) = if all_nodes.len() > 1 {
        (active_nodes.len(), "ノード")
    } else {
        (active.len(), "ディスク")
    };
    (room < usize::from(needed)).then(|| {
        format!(
            "コピーを置ける{unit}が残り {room} 個ですが、最大 {needed} コピーが必要なオブジェクトがあります。容量を追加するまで、それらは draining のディスクから移せません"
        )
    })
}

async fn drain_disk(State(st): State<Admin>, Path(id): Path<String>) -> ApiResult<DrainResult> {
    set_draining(&st, &[parse_disk(&id)?], true).await
}

async fn undrain_disk(State(st): State<Admin>, Path(id): Path<String>) -> ApiResult<DrainResult> {
    set_draining(&st, &[parse_disk(&id)?], false).await
}

#[derive(Deserialize)]
struct ForceQuery {
    #[serde(default)]
    force: bool,
}

#[derive(Serialize)]
struct RemoveResult {
    removed: usize,
    /// Copy records dropped with `force` (those objects were queued for repair).
    dropped: u64,
}

async fn remove_disks(st: &Admin, disks: &[DiskId], force: bool) -> ApiResult<RemoveResult> {
    let meta = st.store.metadata();
    // Check first so that a node is removed whole or not at all.
    if !force {
        for disk in disks {
            let copies = meta.count_disk_replicas(*disk).await?;
            if copies > 0 {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!(
                        "disk {disk} still holds {copies} copies; drain it and let repair finish (or force it if the disk is gone)"
                    ),
                ));
            }
        }
    }
    let mut dropped = 0;
    for disk in disks {
        match st.store.remove_disk(*disk, force).await? {
            RemoveDiskOutcome::Removed { dropped: n } => dropped += n,
            RemoveDiskOutcome::NotFound => return Err(not_found("disk")),
            RemoveDiskOutcome::HasReplicas(n) => {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!("disk {disk} still holds {n} copies"),
                ));
            }
        }
    }
    tracing::info!(target: "suhms::audit", ?disks, force, dropped, "disks removed");
    Ok(Json(RemoveResult {
        removed: disks.len(),
        dropped,
    }))
}

async fn remove_disk(
    State(st): State<Admin>,
    Path(id): Path<String>,
    Query(q): Query<ForceQuery>,
) -> ApiResult<RemoveResult> {
    remove_disks(&st, &[parse_disk(&id)?], q.force).await
}

async fn disks_of(st: &Admin, node: &str) -> Result<Vec<DiskId>, ApiError> {
    let ids: Vec<DiskId> = st
        .store
        .metadata()
        .list_disks()
        .await?
        .into_iter()
        .filter(|d| d.node_id == node)
        .map(|d| d.id)
        .collect();
    if ids.is_empty() {
        return Err(not_found("node"));
    }
    Ok(ids)
}

async fn drain_node(State(st): State<Admin>, Path(id): Path<String>) -> ApiResult<DrainResult> {
    let disks = disks_of(&st, &id).await?;
    set_draining(&st, &disks, true).await
}

async fn undrain_node(State(st): State<Admin>, Path(id): Path<String>) -> ApiResult<DrainResult> {
    let disks = disks_of(&st, &id).await?;
    set_draining(&st, &disks, false).await
}

async fn remove_node(
    State(st): State<Admin>,
    Path(id): Path<String>,
    Query(q): Query<ForceQuery>,
) -> ApiResult<RemoveResult> {
    let disks = disks_of(&st, &id).await?;
    remove_disks(&st, &disks, q.force).await
}

/// Due entries counted for the overview; more are shown as "at least".
const REPAIR_COUNT_LIMIT: u32 = 10_000;

#[derive(Serialize)]
struct Overview {
    node_id: String,
    tenants: usize,
    suspended_tenants: usize,
    objects: u64,
    logical_bytes: u64,
    stored_bytes: u64,
    disks: usize,
    disks_up: usize,
    disks_draining: usize,
    nodes: usize,
    free_bytes: u64,
    repair_due: usize,
    /// `repair_due` stopped counting at the limit.
    repair_due_more: bool,
    allow_public_buckets: bool,
}

async fn overview(State(st): State<Admin>) -> ApiResult<Overview> {
    let meta = st.store.metadata();
    let tenants = meta.list_tenants().await?;
    let usage = meta.storage_usage(None).await?;
    let disks = meta.list_disks().await?;
    let due = meta
        .list_repair_queue(None, REPAIR_COUNT_LIMIT + 1, now_millis())
        .await?
        .len();
    let nodes: std::collections::HashSet<&str> = disks.iter().map(|d| d.node_id.as_str()).collect();
    Ok(Json(Overview {
        node_id: st.store.node_id().to_owned(),
        tenants: tenants.len(),
        suspended_tenants: tenants.iter().filter(|t| t.suspended).count(),
        objects: usage.iter().map(|u| u.objects).sum(),
        logical_bytes: usage.iter().map(|u| u.logical_bytes).sum(),
        stored_bytes: usage.iter().map(|u| u.stored_bytes).sum(),
        disks: disks.len(),
        disks_up: disks.iter().filter(|d| is_up(d)).count(),
        disks_draining: disks.iter().filter(|d| d.draining).count(),
        nodes: nodes.len(),
        free_bytes: disks
            .iter()
            .filter(|d| is_up(d))
            .map(|d| d.free_bytes)
            .sum(),
        repair_due: due.min(REPAIR_COUNT_LIMIT as usize),
        repair_due_more: due > REPAIR_COUNT_LIMIT as usize,
        allow_public_buckets: st.allow_public,
    }))
}

/// What every tenant stores now (tenants without objects are omitted).
#[derive(Serialize)]
struct TenantStorage {
    tenant_id: String,
    objects: u64,
    logical_bytes: u64,
    stored_bytes: u64,
}

async fn storage(State(st): State<Admin>) -> ApiResult<Vec<TenantStorage>> {
    Ok(Json(
        st.store
            .metadata()
            .storage_usage(None)
            .await?
            .into_iter()
            .map(|u| TenantStorage {
                tenant_id: u.tenant_id,
                objects: u.objects,
                logical_bytes: u.logical_bytes,
                stored_bytes: u.stored_bytes,
            })
            .collect(),
    ))
}

async fn list_jobs(State(st): State<Admin>) -> ApiResult<Vec<crate::jobs::JobState>> {
    Ok(Json(st.jobs.list()))
}

async fn start_job(
    State(st): State<Admin>,
    Path(name): Path<String>,
    body: Option<Json<JobParams>>,
) -> Result<StatusCode, ApiError> {
    let params = body.map(|Json(p)| p).unwrap_or_default();
    match st.jobs.start(&name, params) {
        Ok(()) => Ok(StatusCode::ACCEPTED),
        Err(StartError::Unknown) => Err(not_found("job")),
        Err(StartError::Running) => Err(ApiError(
            StatusCode::CONFLICT,
            format!("{name} is already running"),
        )),
    }
}
