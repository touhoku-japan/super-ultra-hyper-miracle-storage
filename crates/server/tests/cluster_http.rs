//! Three real nodes talking over the internal HTTP API (shared SQLite metadata, test only).

mod common;

use std::{path::PathBuf, sync::Arc};

use axum::{Router, body::Body, http::Request};
use common::{Creds, Resp, signed, xml_values};
use suhms_server::{
    admin,
    cluster::{HttpPeers, internal_router},
    config::{ClusterConfig, ClusterTls, Config, NodeConfig, S3Config},
    crypto::SecretCipher,
};
use suhms_store::{Store, StoreConfig};
use tokio::{net::TcpListener, task::JoinHandle};
use tower::ServiceExt;

const SECRET: &str = "0123456789abcdef0123456789abcdef-test";

/// A cluster CA and certificates it signed, written as PEM files.
struct Pki {
    ca_file: PathBuf,
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
}

impl Pki {
    fn new(dir: &std::path::Path, name: &str) -> Self {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, format!("{name} CA"));
        let cert = params.self_signed(&key).unwrap();
        let ca_file = dir.join(format!("{name}-ca.pem"));
        std::fs::write(&ca_file, cert.pem()).unwrap();
        Self {
            ca_file,
            issuer: rcgen::Issuer::new(params, key),
        }
    }

    /// A node certificate valid for 127.0.0.1, usable as server and as client.
    fn node(&self, dir: &std::path::Path, name: &str) -> ClusterTls {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params =
            rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned(), "localhost".to_owned()])
                .unwrap();
        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        let (cert_file, key_file) = (
            dir.join(format!("{name}.pem")),
            dir.join(format!("{name}.key")),
        );
        std::fs::write(&cert_file, cert.pem()).unwrap();
        std::fs::write(&key_file, key.serialize_pem()).unwrap();
        ClusterTls {
            cert_file,
            key_file,
            ca_file: self.ca_file.clone(),
        }
    }
}

struct Node {
    store: Arc<Store>,
    app: Router,
    internal: JoinHandle<()>,
    disks: Vec<PathBuf>,
}

struct Cluster {
    _dir: tempfile::TempDir,
    nodes: Vec<Node>,
    creds: Creds,
    /// Node URLs, and the PKI when the cluster runs over mutual TLS.
    urls: Vec<String>,
    pki: Option<Pki>,
}

impl Cluster {
    async fn start() -> Self {
        Self::start_with(false).await
    }

    async fn start_with(tls: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pki = tls.then(|| Pki::new(dir.path(), "cluster"));
        let scheme = if tls { "https" } else { "http" };
        let meta = format!("sqlite://{}", dir.path().join("meta.db").display());
        let mut listeners = Vec::new();
        let mut nodes_cfg = Vec::new();
        for i in 1..=3 {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            nodes_cfg.push(NodeConfig {
                id: format!("node{i}"),
                url: format!("{scheme}://{}", l.local_addr().unwrap()),
            });
            listeners.push(l);
        }
        let urls: Vec<String> = nodes_cfg.iter().map(|n| n.url.clone()).collect();
        let cipher = Arc::new(SecretCipher::from_hex(&SecretCipher::generate_hex()).unwrap());
        let mut nodes = Vec::new();
        for (i, listener) in listeners.into_iter().enumerate() {
            let id = format!("node{}", i + 1);
            let disks: Vec<PathBuf> = (1..=2)
                .map(|d| {
                    let p = dir.path().join(format!("{id}-d{d}"));
                    std::fs::create_dir(&p).unwrap();
                    p
                })
                .collect();
            let config = Config {
                storage: StoreConfig {
                    node_id: id.clone(),
                    disks: disks.clone(),
                    metadata_url: meta.clone(),
                    ..StoreConfig::default()
                },
                s3: S3Config {
                    region: common::REGION.into(),
                    ..S3Config::default()
                },
                ..Config::default()
            };
            let node_tls = pki.as_ref().map(|p| p.node(dir.path(), &id));
            let cluster = ClusterConfig {
                nodes: nodes_cfg.clone(),
                tls: node_tls.clone(),
                ..ClusterConfig::default()
            };
            let peers = HttpPeers::new(&cluster, &id, SECRET).unwrap();
            let store = Arc::new(
                Store::open_with_peers(config.storage.clone(), Arc::new(peers))
                    .await
                    .unwrap(),
            );
            let router = internal_router(Arc::clone(&store), SECRET);
            let internal = match node_tls {
                Some(t) => {
                    let config = suhms_server::tls::server_config(&t).unwrap();
                    let listener = suhms_server::tls::TlsListener::new(listener, config).unwrap();
                    tokio::spawn(async move {
                        axum::serve(listener, router).await.unwrap();
                    })
                }
                None => tokio::spawn(async move {
                    axum::serve(listener, router).await.unwrap();
                }),
            };
            let app = suhms_server::app(
                Arc::clone(&store),
                suhms_server::tenancy(Arc::clone(&store), &config),
                Arc::clone(&cipher),
                &config,
            )
            .unwrap();
            nodes.push(Node {
                store,
                app,
                internal,
                disks,
            });
        }
        let t = nodes[0].store.create_tenant("alice").await.unwrap();
        let (key, secret) = admin::issue_key(nodes[0].store.metadata().as_ref(), &cipher, &t.id)
            .await
            .unwrap();
        Self {
            _dir: dir,
            nodes,
            creds: Creds { key, secret },
            urls,
            pki,
        }
    }

    async fn s3(
        &self,
        node: usize,
        method: &str,
        path: &str,
        q: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Resp {
        let req = signed(&self.creds, method, path, q, &[], body.into());
        let resp = self.nodes[node].app.clone().oneshot(req).await.unwrap();
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

    fn object_files(&self, node: usize) -> usize {
        fn walk(p: &std::path::Path) -> usize {
            std::fs::read_dir(p).map_or(0, |entries| {
                entries
                    .flatten()
                    .map(|e| {
                        if e.path().is_dir() {
                            walk(&e.path())
                        } else {
                            1
                        }
                    })
                    .sum()
            })
        }
        self.nodes[node]
            .disks
            .iter()
            .map(|d| walk(&d.join("objects")))
            .sum()
    }

    /// Stops a node's internal API and makes its heartbeats stale.
    async fn crash(&self, i: usize) {
        self.nodes[i].internal.abort();
        let node = &self.nodes[i].store;
        for d in node.cluster_disks().await.unwrap().iter() {
            if d.node_id == node.node_id() {
                node.metadata().heartbeat_disk(d.id, 0, 0).await.unwrap();
            }
        }
        for n in &self.nodes {
            n.store.refresh_cluster().await;
        }
    }
}

#[tokio::test]
async fn s3_over_a_three_node_cluster() {
    exercise(Cluster::start().await).await;
}

#[tokio::test]
async fn s3_over_a_three_node_cluster_with_mutual_tls() {
    exercise(Cluster::start_with(true).await).await;
}

#[tokio::test]
async fn cluster_tls_rejects_clients_outside_the_cluster() {
    let c = Cluster::start_with(true).await;
    let dir = tempfile::tempdir().unwrap();
    let pki = c.pki.as_ref().unwrap();
    let url = format!("{}/internal/v1/ping", c.urls[0]);
    let ping = |tls: Option<ClusterTls>, trust: PathBuf| {
        let url = url.clone();
        async move {
            let ca = std::fs::read(&trust).unwrap();
            let mut b = reqwest::Client::builder()
                .tls_certs_only([reqwest::Certificate::from_pem(&ca).unwrap()]);
            if let Some(t) = tls {
                b = b.tls_backend_preconfigured(suhms_server::tls::client_config(&t).unwrap());
            }
            let headers = suhms_server::cluster::auth_header(SECRET, "GET", "/internal/v1/ping");
            b.build().unwrap().get(url).headers(headers).send().await
        }
    };
    // A node certificate from the cluster CA works.
    let ok = ping(Some(pki.node(dir.path(), "admin")), pki.ca_file.clone())
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    // No client certificate: the handshake fails.
    assert!(ping(None, pki.ca_file.clone()).await.is_err());
    // A certificate from another CA: the handshake fails.
    let other = Pki::new(dir.path(), "other");
    let mut foreign = other.node(dir.path(), "intruder");
    foreign.ca_file = pki.ca_file.clone();
    assert!(ping(Some(foreign), pki.ca_file.clone()).await.is_err());
}

#[test]
fn cluster_urls_must_match_the_tls_setting() {
    let node = |url: &str| NodeConfig {
        id: "n1".into(),
        url: url.into(),
    };
    let plain = ClusterConfig {
        nodes: vec![node("https://10.0.0.1:9100")],
        ..ClusterConfig::default()
    };
    assert!(plain.validate().is_err());
    let tls = ClusterConfig {
        nodes: vec![node("http://10.0.0.1:9100")],
        tls: Some(ClusterTls {
            cert_file: "a".into(),
            key_file: "b".into(),
            ca_file: "c".into(),
        }),
        ..ClusterConfig::default()
    };
    assert!(tls.validate().is_err());
}

async fn exercise(c: Cluster) {
    assert_eq!(c.s3(0, "PUT", "/shared", &[], vec![]).await.status, 200);

    let body: Vec<u8> = (0..3_000_000u32).map(|i| (i % 253) as u8).collect();
    let resp = c.s3(0, "PUT", "/shared/obj", &[], body.clone()).await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    for n in 0..3 {
        assert_eq!(c.object_files(n), 1, "node{} holds a plain copy", n + 1);
    }
    // Every node serves it, local copy or not.
    for n in 0..3 {
        let resp = c.s3(n, "GET", "/shared/obj", &[], vec![]).await;
        assert_eq!(resp.status, 200);
        assert!(resp.body.as_ref() == body.as_slice());
    }

    // Multipart through different nodes.
    let resp = c
        .s3(1, "POST", "/shared/mp", &[("uploads", "")], vec![])
        .await;
    let id = xml_values(&resp.text(), "UploadId").remove(0);
    let p1 = vec![7u8; 5 * 1024 * 1024];
    let r1 = c
        .s3(
            2,
            "PUT",
            "/shared/mp",
            &[("partNumber", "1"), ("uploadId", &id)],
            p1.clone(),
        )
        .await;
    let r2 = c
        .s3(
            0,
            "PUT",
            "/shared/mp",
            &[("partNumber", "2"), ("uploadId", &id)],
            b"tail".to_vec(),
        )
        .await;
    assert_eq!(
        (r1.status.as_u16(), r2.status.as_u16()),
        (200, 200),
        "{}",
        r1.text()
    );
    let xml = format!(
        "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part>\
         <Part><PartNumber>2</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>",
        r1.header("etag").unwrap(),
        r2.header("etag").unwrap()
    );
    let resp = c
        .s3(
            1,
            "POST",
            "/shared/mp",
            &[("uploadId", &id)],
            xml.into_bytes(),
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let got = c.s3(2, "GET", "/shared/mp", &[], vec![]).await;
    assert_eq!(got.body.len(), p1.len() + 4);
    for n in 0..3 {
        assert_eq!(c.object_files(n), 2);
    }

    // Node 3 goes away: reads and (degraded) writes continue.
    c.crash(2).await;
    let resp = c.s3(0, "GET", "/shared/obj", &[], vec![]).await;
    assert!(resp.body.as_ref() == body.as_slice());
    let resp = c
        .s3(1, "PUT", "/shared/during-outage", &[], b"x".to_vec())
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    assert_eq!(
        c.nodes[0]
            .store
            .object_replicas("shared", "during-outage")
            .await
            .unwrap()
            .len(),
        2
    );

    // Deletes remove the copies on the reachable nodes.
    assert_eq!(
        c.s3(0, "DELETE", "/shared/obj", &[], vec![]).await.status,
        204
    );
    assert_eq!(c.object_files(0), 2);
    assert_eq!(c.object_files(1), 2);
}

#[tokio::test]
async fn internal_api_requires_the_cluster_secret() {
    let c = Cluster::start().await;
    let router = internal_router(Arc::clone(&c.nodes[0].store), SECRET);

    let resp = router
        .clone()
        .oneshot(
            Request::get("/internal/v1/ping")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    let mut req = Request::get("/internal/v1/ping")
        .body(Body::empty())
        .unwrap();
    *req.headers_mut() =
        suhms_server::cluster::auth_header(&"z".repeat(40), "GET", "/internal/v1/ping");
    assert_eq!(router.clone().oneshot(req).await.unwrap().status(), 403);

    let mut req = Request::get("/internal/v1/ping")
        .body(Body::empty())
        .unwrap();
    *req.headers_mut() = suhms_server::cluster::auth_header(SECRET, "GET", "/internal/v1/ping");
    assert_eq!(router.oneshot(req).await.unwrap().status(), 200);
}
