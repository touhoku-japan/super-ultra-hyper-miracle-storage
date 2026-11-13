//! Administration API for the SoraHost portal: tenants, access keys, quotas, suspension, bucket
//! settings, usage for billing, and cluster status. JSON over HTTP on `[admin] listen`, with a
//! bearer token. `/metrics` (Prometheus) needs no token; keep the listener private.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use serde::{Deserialize, Serialize};
use suhms_store::{BucketRecord, Store, StoreError, TenantRecord, TenantUpdate, UsageRow};

use crate::{admin::issue_key, crypto::SecretCipher, tenancy::Tenancy};

#[derive(Clone)]
struct Admin {
    store: Arc<Store>,
    tenancy: Arc<Tenancy>,
    cipher: Arc<SecretCipher>,
    token: Arc<str>,
    allow_public: bool,
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
    let ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| constant_time_eq(t.as_bytes(), st.token.as_bytes()));
    if !ok {
        return ApiError(StatusCode::UNAUTHORIZED, "invalid admin token".into()).into_response();
    }
    next.run(req).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
    allow_public: bool,
) -> Router {
    let st = Admin {
        store,
        tenancy,
        cipher,
        token: Arc::from(token),
        allow_public,
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
        .layer(middleware::from_fn_with_state(st.clone(), authenticate));
    Router::new()
        .route("/metrics", get(metrics))
        .merge(api)
        .with_state(st)
}
