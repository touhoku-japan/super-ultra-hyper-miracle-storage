//! Three real nodes talking over the internal HTTP API (shared SQLite metadata, test only).

mod common;

use std::{path::PathBuf, sync::Arc};

use axum::{Router, body::Body, http::Request};
use common::{Creds, Resp, signed, xml_values};
use suhms_server::{
    admin,
    cluster::{HttpPeers, internal_router},
    config::{ClusterConfig, Config, NodeConfig, S3Config},
    crypto::SecretCipher,
};
use suhms_store::{Store, StoreConfig};
use tokio::{net::TcpListener, task::JoinHandle};
use tower::ServiceExt;

const SECRET: &str = "0123456789abcdef0123456789abcdef-test";

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
}

impl Cluster {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let meta = format!("sqlite://{}", dir.path().join("meta.db").display());
        let mut listeners = Vec::new();
        let mut nodes_cfg = Vec::new();
        for i in 1..=3 {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            nodes_cfg.push(NodeConfig {
                id: format!("node{i}"),
                url: format!("http://{}", l.local_addr().unwrap()),
            });
            listeners.push(l);
        }
        let cluster = ClusterConfig {
            nodes: nodes_cfg,
            ..ClusterConfig::default()
        };
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
            let peers = HttpPeers::new(&cluster, &id, SECRET).unwrap();
            let store = Arc::new(
                Store::open_with_peers(config.storage.clone(), Arc::new(peers))
                    .await
                    .unwrap(),
            );
            let router = internal_router(Arc::clone(&store), SECRET);
            let internal = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
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
    let c = Cluster::start().await;
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
