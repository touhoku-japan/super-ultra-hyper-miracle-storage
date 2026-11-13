//! Node-to-node API: lets other nodes write, read and delete files on this node's disks.
//!
//! Served on a separate listener (`[cluster] listen`) that should only be reachable on the
//! private network. Every request carries `x-suhms-node-auth: <unix-ms>.<hex HMAC-SHA256>` over
//! the timestamp, method and path+query, keyed with the shared cluster secret; requests older
//! than [`MAX_SKEW_MS`] are refused.

use std::{collections::HashMap, io, ops::Range, sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use suhms_store::{
    BoxReader, Peer, Peers, ReplicaSink, Store,
    blob::{self, Dest, LocalSink},
    disk::{Disk, DiskId},
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::{ReaderStream, StreamReader};
use uuid::Uuid;

use crate::config::ClusterConfig;

const AUTH_HEADER: &str = "x-suhms-node-auth";
const MAX_SKEW_MS: i64 = 5 * 60 * 1000;
const READ_CHUNK: usize = 256 * 1024;

fn now_millis() -> i64 {
    crate::admin::now_millis()
}

fn sign(secret: &str, ts: i64, method: &str, path_and_query: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(format!("{ts}\n{method}\n{path_and_query}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn verify(secret: &str, header: &str, method: &str, path_and_query: &str) -> bool {
    let Some((ts, sig)) = header.split_once('.') else {
        return false;
    };
    let Ok(ts) = ts.parse::<i64>() else {
        return false;
    };
    if (now_millis() - ts).abs() > MAX_SKEW_MS {
        return false;
    }
    let Ok(sig) = hex::decode(sig) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(format!("{ts}\n{method}\n{path_and_query}").as_bytes());
    mac.verify_slice(&sig).is_ok()
}

// ---------------------------------------------------------------- server side

#[derive(Clone)]
struct Internal {
    store: Arc<Store>,
    secret: Arc<str>,
}

async fn authenticate(State(st): State<Internal>, req: Request, next: Next) -> Response {
    let pq = req
        .uri()
        .path_and_query()
        .map_or_else(|| req.uri().path().to_owned(), |p| p.as_str().to_owned());
    let ok = req
        .headers()
        .get(AUTH_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|h| verify(&st.secret, h, req.method().as_str(), &pq));
    if !ok {
        tracing::warn!(method = %req.method(), path = %pq, "rejected unauthenticated node request");
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(req).await
}

fn err(status: StatusCode, e: impl std::fmt::Display) -> Response {
    (status, e.to_string()).into_response()
}

impl Internal {
    fn disk(&self, id: DiskId) -> Result<&Arc<Disk>, (StatusCode, String)> {
        self.store.local_disk(id).ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("disk {id} is not on this node"),
            )
        })
    }
}

/// Streams the request body into a new file; it becomes visible only if the body arrives
/// completely.
async fn receive(st: &Internal, disk: DiskId, dest: Dest, body: Body) -> Response {
    let disk = match st.disk(disk) {
        Ok(d) => d,
        Err(r) => return r.into_response(),
    };
    let mut sink = match LocalSink::create(disk, dest).await {
        Ok(s) => s,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                Box::new(sink).abort().await;
                return err(StatusCode::BAD_REQUEST, e);
            }
        };
        if let Err(e) = sink.write(chunk).await {
            Box::new(sink).abort().await;
            return err(StatusCode::INTERNAL_SERVER_ERROR, e);
        }
    }
    match Box::new(sink).commit().await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn put_object(
    State(st): State<Internal>,
    Path((disk, blob)): Path<(DiskId, Uuid)>,
    body: Body,
) -> Response {
    receive(&st, disk, Dest::Object(blob), body).await
}

async fn put_part(
    State(st): State<Internal>,
    Path((disk, upload, blob)): Path<(DiskId, Uuid, Uuid)>,
    body: Body,
) -> Response {
    receive(&st, disk, Dest::Part { upload, blob }, body).await
}

#[derive(Deserialize)]
struct ReadQuery {
    size: u64,
    start: u64,
    end: u64,
}

async fn get_object(
    State(st): State<Internal>,
    Path((disk, blob)): Path<(DiskId, Uuid)>,
    Query(q): Query<ReadQuery>,
) -> Response {
    let disk = match st.disk(disk) {
        Ok(d) => d,
        Err(r) => return r.into_response(),
    };
    match blob::read_local(disk, blob, q.size, q.start..q.end).await {
        Ok(reader) => {
            Body::from_stream(ReaderStream::with_capacity(reader, READ_CHUNK)).into_response()
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => err(StatusCode::NOT_FOUND, e),
        Err(e) => err(StatusCode::CONFLICT, e),
    }
}

async fn delete_object(
    State(st): State<Internal>,
    Path((disk, blob)): Path<(DiskId, Uuid)>,
) -> Response {
    match st.disk(disk) {
        Ok(d) => {
            blob::remove_path(&d.object_path(blob)).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(r) => r.into_response(),
    }
}

async fn delete_part(
    State(st): State<Internal>,
    Path((disk, upload, blob)): Path<(DiskId, Uuid, Uuid)>,
) -> Response {
    match st.disk(disk) {
        Ok(d) => {
            blob::remove_path(&d.part_path(upload, blob)).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(r) => r.into_response(),
    }
}

async fn delete_upload(
    State(st): State<Internal>,
    Path((disk, upload)): Path<(DiskId, Uuid)>,
) -> Response {
    match st.disk(disk) {
        Ok(d) => {
            blob::remove_upload_dir(d, upload).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(r) => r.into_response(),
    }
}

#[derive(Serialize, Deserialize)]
struct ConcatRequest {
    parts: Vec<(Uuid, String)>,
    object: Uuid,
}

#[derive(Serialize, Deserialize)]
struct ConcatResponse {
    blake3: String,
}

async fn concat(
    State(st): State<Internal>,
    Path((disk, upload)): Path<(DiskId, Uuid)>,
    Json(req): Json<ConcatRequest>,
) -> Response {
    let disk = match st.disk(disk) {
        Ok(d) => d,
        Err(r) => return r.into_response(),
    };
    match blob::concat_parts(disk, upload, &req.parts, req.object).await {
        Ok(blake3) => Json(ConcatResponse { blake3 }).into_response(),
        Err(e) => err(StatusCode::CONFLICT, e),
    }
}

/// The router for the cluster listener.
pub fn internal_router(store: Arc<Store>, secret: &str) -> Router {
    let st = Internal {
        store,
        secret: Arc::from(secret),
    };
    Router::new()
        .route(
            "/internal/v1/disks/{disk}/objects/{blob}",
            put(put_object).get(get_object).delete(delete_object),
        )
        .route(
            "/internal/v1/disks/{disk}/uploads/{upload}/parts/{blob}",
            put(put_part).delete(delete_part),
        )
        .route(
            "/internal/v1/disks/{disk}/uploads/{upload}",
            delete(delete_upload),
        )
        .route(
            "/internal/v1/disks/{disk}/uploads/{upload}/concat",
            post(concat),
        )
        .route("/internal/v1/ping", get(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(st.clone(), authenticate))
        .with_state(st)
}

// ---------------------------------------------------------------- client side

pub struct HttpPeer {
    client: reqwest::Client,
    base: String,
    secret: Arc<str>,
}

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

impl HttpPeer {
    fn path(disk: DiskId, dest: Dest) -> String {
        match dest {
            Dest::Object(blob) => format!("/internal/v1/disks/{disk}/objects/{blob}"),
            Dest::Part { upload, blob } => {
                format!("/internal/v1/disks/{disk}/uploads/{upload}/parts/{blob}")
            }
        }
    }

    fn request(&self, method: Method, path_and_query: &str) -> reqwest::RequestBuilder {
        let ts = now_millis();
        let sig = sign(&self.secret, ts, method.as_str(), path_and_query);
        self.client
            .request(method, format!("{}{path_and_query}", self.base))
            .header(AUTH_HEADER, format!("{ts}.{sig}"))
    }

    async fn check(resp: reqwest::Response) -> io::Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        let kind = if status == StatusCode::NOT_FOUND {
            io::ErrorKind::NotFound
        } else {
            io::ErrorKind::Other
        };
        Err(io::Error::new(
            kind,
            format!("peer returned {status}: {body}"),
        ))
    }
}

/// Streams a write to a peer through a bounded channel feeding the request body.
struct HttpSink {
    tx: Option<mpsc::Sender<io::Result<Bytes>>>,
    response: tokio::task::JoinHandle<reqwest::Result<reqwest::Response>>,
}

#[async_trait]
impl ReplicaSink for HttpSink {
    async fn write(&mut self, chunk: Bytes) -> io::Result<()> {
        let tx = self.tx.as_ref().expect("sink used after finishing");
        tx.send(Ok(chunk))
            .await
            .map_err(|_| io_err("peer stopped receiving"))
    }

    async fn commit(mut self: Box<Self>) -> io::Result<()> {
        drop(self.tx.take());
        let resp = (&mut self.response)
            .await
            .map_err(io_err)?
            .map_err(io_err)?;
        HttpPeer::check(resp).await.map(drop)
    }

    async fn abort(mut self: Box<Self>) {
        // Failing the body makes the peer discard its temp file.
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Err(io_err("write aborted"))).await;
        }
        let _ = (&mut self.response).await;
    }
}

#[async_trait]
impl Peer for HttpPeer {
    async fn open_sink(&self, disk: DiskId, dest: Dest) -> io::Result<Box<dyn ReplicaSink>> {
        let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(4);
        let req = self
            .request(Method::PUT, &Self::path(disk, dest))
            .body(reqwest::Body::wrap_stream(ReceiverStream::new(rx)));
        let response = tokio::spawn(req.send());
        Ok(Box::new(HttpSink {
            tx: Some(tx),
            response,
        }))
    }

    async fn read(
        &self,
        disk: DiskId,
        blob: Uuid,
        size: u64,
        range: Range<u64>,
    ) -> io::Result<BoxReader> {
        let pq = format!(
            "{}?size={size}&start={}&end={}",
            Self::path(disk, Dest::Object(blob)),
            range.start,
            range.end
        );
        let resp = self
            .request(Method::GET, &pq)
            .send()
            .await
            .map_err(io_err)?;
        let resp = Self::check(resp).await?;
        // Pump through a channel so the reader is Sync (required by the S3 response body).
        let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(4);
        let mut stream = resp.bytes_stream().map_err(io_err);
        tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                if tx.send(item).await.is_err() {
                    break;
                }
            }
        });
        Ok(Box::new(StreamReader::new(ReceiverStream::new(rx))))
    }

    async fn remove(&self, disk: DiskId, dest: Dest) -> io::Result<()> {
        let resp = self
            .request(Method::DELETE, &Self::path(disk, dest))
            .send()
            .await
            .map_err(io_err)?;
        Self::check(resp).await.map(drop)
    }

    async fn remove_upload(&self, disk: DiskId, upload: Uuid) -> io::Result<()> {
        let pq = format!("/internal/v1/disks/{disk}/uploads/{upload}");
        let resp = self
            .request(Method::DELETE, &pq)
            .send()
            .await
            .map_err(io_err)?;
        Self::check(resp).await.map(drop)
    }

    async fn concat(
        &self,
        disk: DiskId,
        upload: Uuid,
        parts: Vec<(Uuid, String)>,
        object: Uuid,
    ) -> io::Result<String> {
        let pq = format!("/internal/v1/disks/{disk}/uploads/{upload}/concat");
        let resp = self
            .request(Method::POST, &pq)
            .json(&ConcatRequest { parts, object })
            .send()
            .await
            .map_err(io_err)?;
        let resp = Self::check(resp).await?;
        Ok(resp.json::<ConcatResponse>().await.map_err(io_err)?.blake3)
    }
}

/// The other nodes of the cluster, by node id.
pub struct HttpPeers {
    peers: HashMap<String, Arc<dyn Peer>>,
}

impl HttpPeers {
    pub fn new(config: &ClusterConfig, self_id: &str, secret: &str) -> anyhow::Result<Self> {
        config.validate()?;
        crate::tls::install_provider();
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .tcp_keepalive(Duration::from_secs(30));
        if let Some(tls) = &config.tls {
            builder = builder.tls_backend_preconfigured(crate::tls::client_config(tls)?);
        }
        let client = builder.build()?;
        let secret: Arc<str> = Arc::from(secret);
        let peers = config
            .nodes
            .iter()
            .filter(|n| n.id != self_id)
            .map(|n| {
                let peer: Arc<dyn Peer> = Arc::new(HttpPeer {
                    client: client.clone(),
                    base: n.url.trim_end_matches('/').to_owned(),
                    secret: Arc::clone(&secret),
                });
                (n.id.clone(), peer)
            })
            .collect();
        Ok(Self { peers })
    }
}

impl Peers for HttpPeers {
    fn get(&self, node_id: &str) -> Option<Arc<dyn Peer>> {
        self.peers.get(node_id).cloned()
    }
}

/// Opens the store, as a cluster member when `[cluster] nodes` is configured.
pub async fn open_store(config: &crate::config::Config) -> anyhow::Result<Store> {
    if !config.cluster.is_enabled() {
        return Ok(Store::open(config.storage.clone()).await?);
    }
    anyhow::ensure!(
        config
            .cluster
            .nodes
            .iter()
            .any(|n| n.id == config.storage.node_id),
        "[cluster] nodes does not list this node ({})",
        config.storage.node_id
    );
    anyhow::ensure!(
        !config.storage.metadata_url.starts_with("sqlite:"),
        "a cluster needs a shared metadata database (PostgreSQL or MariaDB), not SQLite"
    );
    let secret = config.cluster.load_secret()?;
    let peers = HttpPeers::new(&config.cluster, &config.storage.node_id, &secret)?;
    Ok(Store::open_with_peers(config.storage.clone(), Arc::new(peers)).await?)
}

/// Unchecked headers helper for tests.
pub fn auth_header(secret: &str, method: &str, path_and_query: &str) -> HeaderMap {
    let ts = now_millis();
    let mut h = HeaderMap::new();
    h.insert(
        AUTH_HEADER,
        format!("{ts}.{}", sign(secret, ts, method, path_and_query))
            .parse()
            .expect("valid header"),
    );
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_bind_method_path_and_time() {
        let s = "x".repeat(32);
        let ts = now_millis();
        let header = format!("{ts}.{}", sign(&s, ts, "GET", "/a?b=1"));
        assert!(verify(&s, &header, "GET", "/a?b=1"));
        assert!(!verify(&s, &header, "PUT", "/a?b=1"));
        assert!(!verify(&s, &header, "GET", "/a?b=2"));
        assert!(!verify(&"y".repeat(32), &header, "GET", "/a?b=1"));
        let old = ts - MAX_SKEW_MS - 1;
        let stale = format!("{old}.{}", sign(&s, old, "GET", "/a"));
        assert!(!verify(&s, &stale, "GET", "/a"));
        assert!(!verify(&s, "garbage", "GET", "/a"));
    }
}
