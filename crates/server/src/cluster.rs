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

/// A failed local write: 507 when the disk is full, so that the writing node can tell (see
/// [`HttpPeer::check`]).
fn write_err(e: &io::Error) -> Response {
    let status = if e.kind() == io::ErrorKind::StorageFull {
        StatusCode::INSUFFICIENT_STORAGE
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    err(status, e)
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
        Err(e) => return write_err(&e),
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
            return write_err(&e);
        }
    }
    match Box::new(sink).commit().await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => write_err(&e),
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

#[derive(Deserialize)]
struct PartQuery {
    size: u64,
}

async fn get_part(
    State(st): State<Internal>,
    Path((disk, upload, blob)): Path<(DiskId, Uuid, Uuid)>,
    Query(q): Query<PartQuery>,
) -> Response {
    let disk = match st.disk(disk) {
        Ok(d) => d,
        Err(r) => return r.into_response(),
    };
    match blob::read_local_path(&disk.part_path(upload, blob), q.size, 0..q.size).await {
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
    /// Part blob and BLAKE3.
    parts: Vec<(Uuid, String)>,
    object: Uuid,
    /// The parts' CRCs, in the same order (missing or `null`: check the BLAKE3). A separate
    /// field so that nodes of the previous version, which ignore it, still assemble.
    #[serde(default)]
    crcs: Vec<Option<blob::PartCrc>>,
}

impl ConcatRequest {
    fn new(parts: Vec<blob::JoinPart>, object: Uuid) -> Self {
        let crcs = parts.iter().map(|p| p.crc).collect();
        Self {
            parts: parts.into_iter().map(|p| (p.blob, p.blake3)).collect(),
            object,
            crcs,
        }
    }

    fn into_parts(self) -> Vec<blob::JoinPart> {
        let mut crcs = self.crcs.into_iter();
        self.parts
            .into_iter()
            .map(|(blob, blake3)| blob::JoinPart {
                blob,
                blake3,
                crc: crcs.next().flatten(),
            })
            .collect()
    }
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
    let object = req.object;
    match blob::concat_parts(disk, upload, &req.into_parts(), object).await {
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
            put(put_part).get(get_part).delete(delete_part),
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

#[derive(Clone)]
pub struct HttpPeer {
    client: reqwest::Client,
    base: String,
    secret: Arc<str>,
    timing: Timing,
}

/// How a peer's liveness is checked while waiting for it (see [`HttpPeer::alive`]).
#[derive(Debug, Clone, Copy)]
struct Timing {
    /// Ping interval while a request is outstanding.
    liveness: Duration,
    /// How long a ping may take.
    ping: Duration,
}

const TIMING: Timing = Timing {
    liveness: LIVENESS_INTERVAL,
    ping: PING_TIMEOUT,
};

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

impl HttpPeer {
    /// Waits for `fut` for as long as the peer stays alive: while it is pending, the peer is
    /// pinged every [`LIVENESS_INTERVAL`], and the wait fails once a ping does.
    ///
    /// Requests to peers have no overall timeout, since a large transfer or assembly can take
    /// long. Without this, a peer that stops responding without closing its connections (a
    /// frozen process, a hung disk, a network that drops packets) would leave the request
    /// waiting forever: the client of the S3 request too, and repair. A peer that is merely
    /// slow still answers pings and is waited for.
    async fn alive<T>(&self, fut: impl std::future::Future<Output = T>) -> io::Result<T> {
        tokio::pin!(fut);
        loop {
            tokio::select! {
                v = &mut fut => return Ok(v),
                r = async {
                    tokio::time::sleep(self.timing.liveness).await;
                    self.ping_once().await
                } => {
                    if let Err(e) = r {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("peer stopped responding: {e}"),
                        ));
                    }
                }
            }
        }
    }

    async fn ping_once(&self) -> io::Result<()> {
        let resp = self
            .request(Method::GET, "/internal/v1/ping")
            .timeout(self.timing.ping)
            .send()
            .await
            .map_err(io_err)?;
        Self::check(resp).await.map(drop)
    }

    /// Sends a request and checks its status, for as long as the peer stays alive.
    async fn send(&self, req: reqwest::RequestBuilder) -> io::Result<reqwest::Response> {
        self.alive(async { Self::check(req.send().await.map_err(io_err)?).await })
            .await?
    }

    /// GETs a file from the peer as a reader.
    async fn get_stream(&self, path_and_query: &str) -> io::Result<BoxReader> {
        let resp = self.send(self.request(Method::GET, path_and_query)).await?;
        // Pump through a channel so the reader is Sync (required by the S3 response body).
        let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(4);
        let mut stream = resp.bytes_stream().map_err(io_err);
        let peer = self.clone();
        tokio::spawn(async move {
            loop {
                let item = match peer.alive(stream.next()).await {
                    Ok(Some(item)) => item,
                    Ok(None) => break,
                    Err(e) => Err(e),
                };
                let failed = item.is_err();
                if tx.send(item).await.is_err() || failed {
                    break;
                }
            }
        });
        Ok(Box::new(StreamReader::new(ReceiverStream::new(rx))))
    }

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
        let kind = match status {
            StatusCode::NOT_FOUND => io::ErrorKind::NotFound,
            StatusCode::INSUFFICIENT_STORAGE => io::ErrorKind::StorageFull,
            _ => io::ErrorKind::Other,
        };
        Err(io::Error::new(
            kind,
            format!("peer returned {status}: {body}"),
        ))
    }
}

/// Streams a write to a peer through a bounded channel feeding the request body. Every wait on
/// the peer is bounded by its liveness ([`HttpPeer::alive`]).
struct HttpSink {
    peer: HttpPeer,
    tx: Option<mpsc::Sender<io::Result<Bytes>>>,
    /// Taken once awaited (a finished `JoinHandle` must not be polled again).
    response: Option<tokio::task::JoinHandle<reqwest::Result<reqwest::Response>>>,
}

impl HttpSink {
    /// Waits for the peer's response to the upload and checks its status.
    async fn finish(&mut self) -> io::Result<()> {
        let Some(mut response) = self.response.take() else {
            return Err(io_err("peer response already consumed"));
        };
        let resp = match self.peer.alive(&mut response).await {
            Ok(r) => r.map_err(io_err)?.map_err(io_err)?,
            Err(e) => {
                response.abort();
                return Err(e);
            }
        };
        self.peer.alive(HttpPeer::check(resp)).await?.map(drop)
    }
}

#[async_trait]
impl ReplicaSink for HttpSink {
    async fn write(&mut self, chunk: Bytes) -> io::Result<()> {
        let tx = self.tx.as_ref().expect("sink used after finishing");
        if self.peer.alive(tx.send(Ok(chunk))).await?.is_ok() {
            return Ok(());
        }
        // The peer gave up on the body: its response says why (a full disk, say).
        self.tx = None;
        match self.finish().await {
            Err(e) => Err(e),
            Ok(()) => Err(io_err("peer stopped receiving")),
        }
    }

    async fn commit(mut self: Box<Self>) -> io::Result<()> {
        drop(self.tx.take());
        self.finish().await
    }

    async fn abort(mut self: Box<Self>) {
        let this = &mut *self;
        // Failing the body makes the peer discard its temp file.
        if let Some(tx) = this.tx.take() {
            let _ = this.peer.alive(tx.send(Err(io_err("write aborted")))).await;
        }
        if let Some(mut response) = this.response.take()
            && this.peer.alive(&mut response).await.is_err()
        {
            response.abort();
        }
    }
}

#[async_trait]
impl Peer for HttpPeer {
    async fn open_sink(&self, disk: DiskId, dest: Dest) -> io::Result<Box<dyn ReplicaSink>> {
        let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(4);
        // hyper polls the body only once the request is on a connection, so the first poll tells
        // that the peer is reachable. Waiting for it lets a dead peer fail here, before any of
        // the object is read, where the caller can still pick another disk.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let mut ready_tx = Some(ready_tx);
        let mut rx = ReceiverStream::new(rx);
        let body = futures::stream::poll_fn(move |cx| {
            if let Some(t) = ready_tx.take() {
                let _ = t.send(());
            }
            rx.poll_next_unpin(cx)
        });
        let req = self
            .request(Method::PUT, &Self::path(disk, dest))
            .body(reqwest::Body::wrap_stream(body));
        let mut response = tokio::spawn(req.send());
        tokio::select! {
            r = ready_rx => {
                if r.is_err() {
                    // The request was dropped before sending its body.
                    return Err(io_err("peer request ended before sending the body"));
                }
            }
            r = &mut response => {
                return Err(match r {
                    Ok(Err(e)) => io_err(e),
                    Ok(Ok(resp)) if resp.status() == StatusCode::INSUFFICIENT_STORAGE => {
                        io::Error::new(io::ErrorKind::StorageFull, "peer disk is full")
                    }
                    Ok(Ok(resp)) => io_err(format!("peer answered {} before the body", resp.status())),
                    Err(e) => io_err(e),
                });
            }
            () = tokio::time::sleep(OPEN_TIMEOUT) => {
                response.abort();
                return Err(io_err("timed out connecting to peer"));
            }
        }
        Ok(Box::new(HttpSink {
            peer: self.clone(),
            tx: Some(tx),
            response: Some(response),
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
        self.get_stream(&pq).await
    }

    async fn read_part(
        &self,
        disk: DiskId,
        upload: Uuid,
        blob: Uuid,
        size: u64,
    ) -> io::Result<BoxReader> {
        let pq = format!(
            "{}?size={size}",
            Self::path(disk, Dest::Part { upload, blob })
        );
        self.get_stream(&pq).await
    }

    async fn remove(&self, disk: DiskId, dest: Dest) -> io::Result<()> {
        self.send(self.request(Method::DELETE, &Self::path(disk, dest)))
            .await
            .map(drop)
    }

    async fn ping(&self) -> io::Result<()> {
        self.ping_once().await
    }

    async fn remove_upload(&self, disk: DiskId, upload: Uuid) -> io::Result<()> {
        let pq = format!("/internal/v1/disks/{disk}/uploads/{upload}");
        self.send(self.request(Method::DELETE, &pq)).await.map(drop)
    }

    async fn concat(
        &self,
        disk: DiskId,
        upload: Uuid,
        parts: Vec<blob::JoinPart>,
        object: Uuid,
    ) -> io::Result<String> {
        let pq = format!("/internal/v1/disks/{disk}/uploads/{upload}/concat");
        let req = self
            .request(Method::POST, &pq)
            .json(&ConcatRequest::new(parts, object));
        // Assembling a large object takes a while; the peer keeps answering pings meanwhile.
        let resp = self.send(req).await?;
        Ok(self
            .alive(resp.json::<ConcatResponse>())
            .await?
            .map_err(io_err)?
            .blake3)
    }
}

/// The other nodes of the cluster, by node id.
/// How long a peer may take to answer a ping before it counts as unreachable.
const PING_TIMEOUT: Duration = Duration::from_secs(3);
/// How long opening a remote copy may wait for a connection to the peer.
const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
/// How often a peer is pinged while a request to it is outstanding ([`HttpPeer::alive`]).
const LIVENESS_INTERVAL: Duration = Duration::from_secs(5);

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
                    timing: TIMING,
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

    /// A peer for `base` that checks liveness quickly.
    fn fast_peer(base: String) -> HttpPeer {
        crate::tls::install_provider();
        HttpPeer {
            client: reqwest::Client::new(),
            base,
            secret: Arc::from("x".repeat(32)),
            timing: Timing {
                liveness: Duration::from_millis(100),
                ping: Duration::from_millis(300),
            },
        }
    }

    /// A "peer" that accepts connections and never answers (like a frozen process: the kernel
    /// still completes the TCP handshake).
    async fn black_hole() -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = l.accept().await {
                held.push(s);
            }
        });
        format!("http://{addr}")
    }

    async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), f)
            .await
            .expect("must fail, not hang")
    }

    #[tokio::test]
    async fn requests_to_a_peer_that_stopped_responding_fail_instead_of_hanging() {
        let peer = fast_peer(black_hole().await);
        let disk = Uuid::new_v4();
        let blob = Uuid::new_v4();

        // The body is accepted into socket buffers, so opening and writing succeed; the commit
        // waits for an answer that never comes.
        let mut sink = peer.open_sink(disk, Dest::Object(blob)).await.unwrap();
        sink.write(Bytes::from_static(b"data")).await.unwrap();
        let err = within(sink.commit()).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");

        let err = within(peer.read(disk, blob, 4, 0..4)).await.err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        within(peer.remove(disk, Dest::Object(blob)))
            .await
            .unwrap_err();
        within(peer.concat(disk, Uuid::new_v4(), Vec::new(), blob))
            .await
            .unwrap_err();
        peer.ping().await.unwrap_err();
    }

    #[tokio::test]
    async fn a_slow_peer_that_answers_pings_is_waited_for() {
        use axum::routing::put;
        let app = Router::new()
            .route("/internal/v1/ping", get(|| async { "ok" }))
            .route(
                "/internal/v1/disks/{disk}/objects/{blob}",
                put(|body: Body| async move {
                    let _ = axum::body::to_bytes(body, usize::MAX).await;
                    // Much longer than the ping interval (a slow fsync, say).
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    StatusCode::OK
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await });
        let peer = fast_peer(format!("http://{addr}"));
        let mut sink = peer
            .open_sink(Uuid::new_v4(), Dest::Object(Uuid::new_v4()))
            .await
            .unwrap();
        sink.write(Bytes::from_static(b"data")).await.unwrap();
        sink.commit().await.unwrap();
    }

    /// A peer whose disk fills up mid-write answers 507; the writer sees `StorageFull` (from the
    /// write that finds the body refused, or from the commit), so the write is reported as a lack
    /// of space rather than a failing node.
    #[tokio::test]
    async fn a_peer_out_of_space_is_reported_as_storage_full() {
        use axum::routing::put;
        use futures::StreamExt as _;
        let app = Router::new()
            .route("/internal/v1/ping", get(|| async { "ok" }))
            .route(
                "/internal/v1/disks/{disk}/objects/{blob}",
                put(|body: Body| async move {
                    // Takes the first chunk, then stops: the disk is full.
                    let _ = body.into_data_stream().next().await;
                    write_err(&io::Error::from(io::ErrorKind::StorageFull))
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await });
        let peer = fast_peer(format!("http://{addr}"));
        let mut sink = peer
            .open_sink(Uuid::new_v4(), Dest::Object(Uuid::new_v4()))
            .await
            .unwrap();
        let chunk = Bytes::from(vec![0u8; 1 << 20]);
        let mut failed = None;
        for _ in 0..64 {
            if let Err(e) = within(sink.write(chunk.clone())).await {
                failed = Some(e);
                break;
            }
        }
        let err = match failed {
            Some(e) => {
                sink.abort().await;
                e
            }
            None => within(sink.commit()).await.unwrap_err(),
        };
        assert_eq!(err.kind(), io::ErrorKind::StorageFull, "{err}");
    }

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
