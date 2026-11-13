//! Server library: configuration, the S3 front end and administration commands. The `suhms`
//! binary is a thin wrapper; integration tests drive [`app`] directly.

pub mod admin;
pub mod admin_api;
pub mod checksum;
pub mod cluster;
pub mod config;
pub mod crypto;
pub mod s3;
pub mod tenancy;

use std::{convert::Infallible, sync::Arc};

use axum::{
    Router,
    body::Body,
    http::{Request, Response, StatusCode},
    routing::get,
};
use suhms_store::Store;
use tower_http::trace::TraceLayer;

use crate::{config::Config, crypto::SecretCipher, tenancy::Tenancy};

/// Health check path. Bucket names cannot start with `_`, so this never shadows a bucket.
pub const HEALTH_PATH: &str = "/_suhms/health";

/// Builds the HTTP application: the health check plus the S3 API for every other path.
pub fn app(
    store: Arc<Store>,
    tenancy: Arc<Tenancy>,
    cipher: Arc<SecretCipher>,
    config: &Config,
) -> anyhow::Result<Router> {
    let mut s3_config = s3s::config::S3Config::default();
    s3_config.put_object_max_size = Some(config.s3.max_object_size);
    // Signed aws-chunked chunks are buffered whole; SDKs use 64 KiB, so 16 MiB is generous.
    s3_config.aws_chunked_stream_max_chunk_size = 16 * 1024 * 1024;
    // Browser form uploads (POST Object) are not offered yet.
    s3_config.allow_post_signature = false;

    let mut builder = s3s::service::S3ServiceBuilder::new(s3::S3Service::new(
        Arc::clone(&store),
        Arc::clone(&tenancy),
        config.s3.region.clone(),
    ));
    builder.set_config(Arc::new(s3s::config::StaticConfigProvider::new(Arc::new(
        s3_config,
    ))));
    builder.set_auth(s3::KeyStore::new(Arc::clone(&tenancy), cipher));
    builder.set_access(s3::PublicAccess {
        allow_public: config.s3.allow_public_buckets,
    });
    if !config.s3.domains.is_empty() {
        builder.set_host(s3s::host::MultiDomain::new(&config.s3.domains)?);
    }
    let s3_service = builder.build();

    let fallback = tower::service_fn(move |req: Request<Body>| {
        let mut svc = s3_service.clone();
        async move {
            let response = match tower::Service::call(&mut svc, req).await {
                Ok(resp) => resp.map(Body::new),
                Err(error) => {
                    tracing::error!(?error, "S3 service failed");
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::empty())
                        .expect("static response is valid")
                }
            };
            Ok::<_, Infallible>(response)
        }
    });

    Ok(Router::new()
        .route(HEALTH_PATH, get(|| async { "ok" }))
        .fallback_service(fallback)
        .layer(axum::middleware::from_fn(audit))
        .layer(TraceLayer::new_for_http()))
}

/// The tenancy services (key cache, rate limits, meter) for an app.
pub fn tenancy(store: Arc<Store>, config: &Config) -> Arc<Tenancy> {
    Arc::new(Tenancy::new(
        store,
        &config.limits,
        config.s3.allow_public_buckets,
    ))
}

/// Access key id of a SigV4 request (header or presigned), for the audit log.
fn access_key_of(req: &Request<Body>) -> Option<String> {
    if let Some(auth) = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        && let Some(rest) = auth.split("Credential=").nth(1)
    {
        return rest.split('/').next().map(str::to_owned);
    }
    let query = req.uri().query()?;
    query.split('&').find_map(|kv| {
        let v = kv.strip_prefix("X-Amz-Credential=")?;
        v.split("%2F").next().map(str::to_owned)
    })
}

/// Writes one `suhms::audit` log line per S3 request: who, what, and the outcome.
async fn audit(req: Request<Body>, next: axum::middleware::Next) -> Response<Body> {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let key = access_key_of(&req);
    let started = std::time::Instant::now();
    let resp = next.run(req).await;
    tracing::info!(
        target: "suhms::audit",
        %method,
        path,
        access_key = key.as_deref().unwrap_or("-"),
        status = resp.status().as_u16(),
        ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "s3 request"
    );
    resp
}
