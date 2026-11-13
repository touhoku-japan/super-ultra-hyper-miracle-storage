//! Mutual TLS for node-to-node traffic.
//!
//! Every node has a certificate signed by a private cluster CA. The cluster listener only
//! accepts clients presenting such a certificate, and nodes only trust servers presenting one,
//! so neither side talks to anything outside the cluster. The HMAC header on each request
//! (see `cluster.rs`) stays as a second, independent check.

use std::{io, net::SocketAddr, path::Path, sync::Arc, time::Duration};

use anyhow::Context as _;
use rustls::{
    ClientConfig, RootCertStore, ServerConfig, crypto::ring, server::WebPkiClientVerifier,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};

use crate::config::ClusterTls;

/// A handshake that has not finished by then is dropped, so a stalled client cannot hold a
/// connection slot open.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// Makes `ring` the process-wide TLS provider (reqwest needs one even for plain-HTTP clients).
/// Safe to call more than once.
pub fn install_provider() {
    let _ = ring::default_provider().install_default();
}

fn certs(path: &Path) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_file_iter(path)
        .with_context(|| format!("failed to read {}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("invalid certificate in {}", path.display()))?;
    anyhow::ensure!(!certs.is_empty(), "no certificate in {}", path.display());
    Ok(certs)
}

fn key(path: &Path) -> anyhow::Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_file(path)
        .with_context(|| format!("no usable private key in {}", path.display()))
}

fn roots(path: &Path) -> anyhow::Result<Arc<RootCertStore>> {
    let mut store = RootCertStore::empty();
    for cert in certs(path)? {
        store
            .add(cert)
            .with_context(|| format!("invalid CA certificate in {}", path.display()))?;
    }
    Ok(Arc::new(store))
}

/// Server side: requires a client certificate signed by the cluster CA.
pub fn server_config(tls: &ClusterTls) -> anyhow::Result<Arc<ServerConfig>> {
    let verifier =
        WebPkiClientVerifier::builder_with_provider(roots(&tls.ca_file)?, provider()).build()?;
    let config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs(&tls.cert_file)?, key(&tls.key_file)?)
        .context("node certificate and key do not match")?;
    Ok(Arc::new(config))
}

/// Client side: trusts only the cluster CA and presents this node's certificate.
pub fn client_config(tls: &ClusterTls) -> anyhow::Result<ClientConfig> {
    ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots(&tls.ca_file)?)
        .with_client_auth_cert(certs(&tls.cert_file)?, key(&tls.key_file)?)
        .context("node certificate and key do not match")
}

/// A listener for `axum::serve` that completes TLS handshakes in the background, so one slow
/// or failing handshake does not hold up other connections.
pub struct TlsListener {
    rx: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local: SocketAddr,
}

impl TlsListener {
    pub fn new(tcp: TcpListener, config: Arc<ServerConfig>) -> io::Result<Self> {
        let local = tcp.local_addr()?;
        let acceptor = TlsAcceptor::from(config);
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                let (stream, peer) = match tcp.accept().await {
                    Ok(c) => c,
                    Err(error) => {
                        tracing::warn!(%error, "cluster accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                if tx.is_closed() {
                    return;
                }
                let (acceptor, conn_tx) = (acceptor.clone(), tx.clone());
                tokio::spawn(async move {
                    match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                        Ok(Ok(tls)) => {
                            let _ = conn_tx.send((tls, peer)).await;
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(%peer, %error, "cluster TLS handshake failed");
                        }
                        Err(_) => tracing::warn!(%peer, "cluster TLS handshake timed out"),
                    }
                });
            }
        });
        Ok(Self { rx, local })
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(conn) => conn,
            // The accept task only ends when this listener is dropped.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.local)
    }
}
