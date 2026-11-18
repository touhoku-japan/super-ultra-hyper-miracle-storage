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

/// A node certificate that expires within this many seconds is warned about.
pub const CERT_WARN_SECS: i64 = 30 * 86_400;

/// Reads this node's certificate (the first one in `cert_file`) and checks that it is valid
/// now. An expired (or not yet valid) certificate is an error: other nodes would reject it and
/// the node would be cut off from the cluster. Returns its expiry (UNIX seconds) and warns
/// when it is near.
pub fn check_node_cert(tls: &ClusterTls) -> anyhow::Result<i64> {
    let cert = certs(&tls.cert_file)?.remove(0);
    let (not_before, not_after) = cert_validity(&cert).with_context(|| {
        format!(
            "cannot read the validity of the certificate in {}",
            tls.cert_file.display()
        )
    })?;
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    anyhow::ensure!(
        now <= not_after,
        "node certificate {} expired at {}; renew it (docs/cluster-tls.md)",
        tls.cert_file.display(),
        fmt_time(not_after)
    );
    anyhow::ensure!(
        now >= not_before,
        "node certificate {} is not valid before {} (check the clock)",
        tls.cert_file.display(),
        fmt_time(not_before)
    );
    warn_if_expiring(not_after, now);
    Ok(not_after)
}

/// Logs a warning while the certificate is close to expiry (an error once it has expired).
pub fn warn_if_expiring(not_after: i64, now: i64) {
    let left = not_after - now;
    if left < 0 {
        tracing::error!(expired_at = %fmt_time(not_after),
            "node certificate has expired; other nodes reject this node until it is renewed and the node restarted");
    } else if left < CERT_WARN_SECS {
        tracing::warn!(expires_at = %fmt_time(not_after), days_left = left / 86_400,
            "node certificate expires soon; renew it (docs/cluster-tls.md)");
    }
}

fn fmt_time(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .map_or_else(|_| unix.to_string(), |t| t.to_string())
}

/// `notBefore` and `notAfter` of a DER certificate, as UNIX seconds.
///
/// A minimal DER walk (Certificate → TBSCertificate → Validity) so that no X.509 parser is
/// needed just for this.
pub fn cert_validity(der: &[u8]) -> anyhow::Result<(i64, i64)> {
    let (cert, _) = der_take(der, 0x30)?;
    let (mut tbs, _) = der_take(cert, 0x30)?;
    if tbs.first() == Some(&0xa0) {
        tbs = der_take(tbs, 0xa0)?.1; // version
    }
    tbs = der_take(tbs, 0x02)?.1; // serialNumber
    tbs = der_take(tbs, 0x30)?.1; // signature
    tbs = der_take(tbs, 0x30)?.1; // issuer
    let (validity, _) = der_take(tbs, 0x30)?;
    let (not_before, rest) = der_time(validity)?;
    let (not_after, _) = der_time(rest)?;
    Ok((not_before, not_after))
}

/// Splits one element with tag `tag` off `input`: (its contents, the rest).
fn der_take(input: &[u8], tag: u8) -> anyhow::Result<(&[u8], &[u8])> {
    let (&t, rest) = input.split_first().context("truncated DER")?;
    anyhow::ensure!(t == tag, "unexpected DER tag {t:#x} (wanted {tag:#x})");
    let (&first, mut rest) = rest.split_first().context("truncated DER")?;
    let len = if first < 0x80 {
        usize::from(first)
    } else {
        let n = usize::from(first & 0x7f);
        anyhow::ensure!((1..=4).contains(&n) && rest.len() >= n, "bad DER length");
        let len = rest[..n]
            .iter()
            .fold(0usize, |a, b| (a << 8) | usize::from(*b));
        rest = &rest[n..];
        len
    };
    anyhow::ensure!(rest.len() >= len, "truncated DER");
    Ok(rest.split_at(len))
}

/// Reads a UTCTime (`YYMMDDHHMMSSZ`) or GeneralizedTime (`YYYYMMDDHHMMSSZ`).
fn der_time(input: &[u8]) -> anyhow::Result<(i64, &[u8])> {
    let tag = *input.first().context("truncated DER")?;
    let (body, rest) = der_take(input, tag)?;
    let text = std::str::from_utf8(body).context("bad time")?;
    let digits = text.strip_suffix('Z').context("time is not UTC")?;
    let (year, digits) = match (tag, digits.len()) {
        (0x17, 12) => {
            let yy: i32 = digits[..2].parse()?;
            // RFC 5280: 50..99 are 19xx, 00..49 are 20xx.
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, &digits[2..])
        }
        (0x18, 14) => (digits[..4].parse()?, &digits[4..]),
        _ => anyhow::bail!("unsupported time {text:?}"),
    };
    let num = |i: usize| -> anyhow::Result<u8> { Ok(digits[i..i + 2].parse()?) };
    let date = time::Date::from_calendar_date(year, time::Month::try_from(num(0)?)?, num(2)?)?;
    let at = time::Time::from_hms(num(4)?, num(6)?, num(8)?)?;
    Ok((
        time::PrimitiveDateTime::new(date, at)
            .assume_utc()
            .unix_timestamp(),
        rest,
    ))
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
                // See `nodelay` in main.rs: avoids a delayed-ACK stall on small responses.
                if let Err(error) = stream.set_nodelay(true) {
                    tracing::warn!(%peer, %error, "cannot set TCP_NODELAY");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cert(dir: &Path, not_before: (i32, u8, u8), not_after: (i32, u8, u8)) -> ClusterTls {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
        params.not_before = rcgen::date_time_ymd(not_before.0, not_before.1, not_before.2);
        params.not_after = rcgen::date_time_ymd(not_after.0, not_after.1, not_after.2);
        let cert = params.self_signed(&key).unwrap();
        let cert_file = dir.join(format!("{}.pem", not_after.0));
        std::fs::write(&cert_file, cert.pem()).unwrap();
        ClusterTls {
            cert_file: cert_file.clone(),
            key_file: cert_file.clone(),
            ca_file: cert_file,
        }
    }

    #[test]
    fn reads_validity_from_der() {
        let dir = tempfile::tempdir().unwrap();
        // Before 2050 rcgen writes UTCTime, from 2050 on GeneralizedTime.
        let tls = cert(dir.path(), (2020, 1, 2), (2060, 3, 4));
        let der = certs(&tls.cert_file).unwrap().remove(0);
        assert_eq!(cert_validity(&der).unwrap(), (1_577_923_200, 2_845_584_000));
        assert!(cert_validity(&der[..40]).is_err());
    }

    #[test]
    fn expired_or_future_certificates_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = check_node_cert(&cert(dir.path(), (2020, 1, 1), (2021, 1, 1))).unwrap_err();
        assert!(err.to_string().contains("expired"), "{err}");
        let err = check_node_cert(&cert(dir.path(), (2090, 1, 1), (2099, 1, 1))).unwrap_err();
        assert!(err.to_string().contains("not valid before"), "{err}");
        let ok = check_node_cert(&cert(dir.path(), (2020, 1, 1), (2098, 1, 1))).unwrap();
        assert!(ok > time::OffsetDateTime::now_utc().unix_timestamp());
    }
}
