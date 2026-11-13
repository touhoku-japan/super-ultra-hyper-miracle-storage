//! Server library: configuration, the S3 front end and administration commands. The `suhms`
//! binary is a thin wrapper; integration tests drive [`app`] directly.

pub mod admin;
pub mod checksum;
pub mod config;
pub mod crypto;
pub mod s3;

use std::{convert::Infallible, sync::Arc};

use axum::{
    Router,
    body::Body,
    http::{Request, Response, StatusCode},
    routing::get,
};
use suhms_store::Store;
use tower_http::trace::TraceLayer;

use crate::{config::Config, crypto::SecretCipher};

/// Health check path. Bucket names cannot start with `_`, so this never shadows a bucket.
pub const HEALTH_PATH: &str = "/_suhms/health";

/// Builds the HTTP application: the health check plus the S3 API for every other path.
pub fn app(
    store: Arc<Store>,
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
        config.s3.region.clone(),
    ));
    builder.set_config(Arc::new(s3s::config::StaticConfigProvider::new(Arc::new(
        s3_config,
    ))));
    builder.set_auth(s3::KeyStore::new(store, cipher));
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
        .layer(TraceLayer::new_for_http()))
}
