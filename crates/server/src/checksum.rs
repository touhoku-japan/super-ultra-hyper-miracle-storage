//! Verification of `x-amz-checksum-*` on uploads.
//!
//! The expected value comes either from a request header or, for aws-chunked uploads, from a
//! trailer named by `x-amz-trailer` (what current AWS SDKs send by default). The body is hashed
//! while it streams into the store; at end of stream the reader compares and, on mismatch,
//! fails the read so the store discards the write before anything is committed.

use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, ready},
};

use s3s::{
    S3Error, S3Result,
    checksum::ChecksumHasher,
    crypto::{Checksum as _, Crc32, Crc32c, Crc64Nvme, Sha1, Sha256},
    dto::Checksum,
    s3_error,
};
use tokio::io::{AsyncRead, ReadBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algo {
    Crc32,
    Crc32c,
    Crc64Nvme,
    Sha1,
    Sha256,
}

const ALGOS: [(Algo, &str); 5] = [
    (Algo::Crc32, "x-amz-checksum-crc32"),
    (Algo::Crc32c, "x-amz-checksum-crc32c"),
    (Algo::Crc64Nvme, "x-amz-checksum-crc64nvme"),
    (Algo::Sha1, "x-amz-checksum-sha1"),
    (Algo::Sha256, "x-amz-checksum-sha256"),
];

impl Algo {
    fn hasher(self) -> ChecksumHasher {
        let mut h = ChecksumHasher::default();
        match self {
            Self::Crc32 => h.crc32 = Some(Crc32::new()),
            Self::Crc32c => h.crc32c = Some(Crc32c::new()),
            Self::Crc64Nvme => h.crc64nvme = Some(Crc64Nvme::new()),
            Self::Sha1 => h.sha1 = Some(Sha1::new()),
            Self::Sha256 => h.sha256 = Some(Sha256::new()),
        }
        h
    }

    fn value(self, c: &Checksum) -> Option<&str> {
        match self {
            Self::Crc32 => c.checksum_crc32.as_deref(),
            Self::Crc32c => c.checksum_crc32c.as_deref(),
            Self::Crc64Nvme => c.checksum_crc64nvme.as_deref(),
            Self::Sha1 => c.checksum_sha1.as_deref(),
            Self::Sha256 => c.checksum_sha256.as_deref(),
        }
    }
}

#[derive(Debug, Clone)]
enum Source {
    Header(String),
    Trailer(s3s::TrailingHeaders, &'static str),
}

/// What the client claims the body's checksum is.
#[derive(Debug, Clone)]
pub struct Expected {
    algo: Algo,
    source: Source,
}

/// Finds the checksum the client sent, if any.
pub fn expected(
    headers: &axum::http::HeaderMap,
    trailers: Option<&s3s::TrailingHeaders>,
) -> S3Result<Option<Expected>> {
    let mut found = None;
    for (algo, name) in ALGOS {
        if let Some(v) = headers.get(name) {
            if found.is_some() {
                return Err(s3_error!(
                    InvalidRequest,
                    "Expecting a single x-amz-checksum- header"
                ));
            }
            let v = v
                .to_str()
                .map_err(|_| s3_error!(InvalidRequest, "invalid checksum header"))?;
            found = Some(Expected {
                algo,
                source: Source::Header(v.to_owned()),
            });
        }
    }
    if found.is_some() {
        return Ok(found);
    }
    let Some(trailer) = headers.get("x-amz-trailer") else {
        return Ok(None);
    };
    let trailer = trailer
        .to_str()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if let Some((algo, name)) = ALGOS.iter().find(|(_, n)| *n == trailer) {
        let trailers = trailers.ok_or_else(|| {
            s3_error!(InvalidRequest, "x-amz-trailer requires an aws-chunked body")
        })?;
        return Ok(Some(Expected {
            algo: *algo,
            source: Source::Trailer(trailers.clone(), name),
        }));
    }
    if trailer.starts_with("x-amz-checksum-") {
        return Err(s3_error!(InvalidRequest, "unsupported checksum algorithm"));
    }
    Ok(None)
}

/// Where the verified checksum is published once the body has been read completely.
pub type Outcome = Arc<Mutex<Option<Checksum>>>;

pub struct VerifyingReader<R> {
    inner: R,
    expected: Expected,
    hasher: Option<ChecksumHasher>,
    outcome: Outcome,
}

/// Wraps `inner` so that its checksum is verified at end of stream.
pub fn wrap<R>(inner: R, expected: Option<Expected>) -> (Box<dyn AsyncRead + Unpin + Send>, Outcome)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let outcome = Outcome::default();
    let reader: Box<dyn AsyncRead + Unpin + Send> = match expected {
        None => Box::new(inner),
        Some(expected) => Box::new(VerifyingReader {
            inner,
            hasher: Some(expected.algo.hasher()),
            expected,
            outcome: Arc::clone(&outcome),
        }),
    };
    (reader, outcome)
}

fn mismatch(algo: Algo) -> io::Error {
    let name = format!("{algo:?}").to_ascii_uppercase();
    io::Error::other(S3Error::with_message(
        s3s::S3ErrorCode::BadDigest,
        format!("The {name} you specified did not match the calculated checksum."),
    ))
}

impl<R: AsyncRead + Unpin> AsyncRead for VerifyingReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        ready!(Pin::new(&mut self.inner).poll_read(cx, buf))?;
        let new = &buf.filled()[before..];
        let this = &mut *self;
        if !new.is_empty() {
            if let Some(h) = this.hasher.as_mut() {
                h.update(new);
            }
            return Poll::Ready(Ok(()));
        }
        // End of stream: trailers (if any) have been parsed by now.
        if let Some(h) = this.hasher.take() {
            let computed = h.finalize();
            let algo = this.expected.algo;
            let claimed = match &this.expected.source {
                Source::Header(v) => Some(v.clone()),
                Source::Trailer(t, name) => t
                    .read(|h| {
                        h.get(*name)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned)
                    })
                    .flatten(),
            };
            let Some(claimed) = claimed else {
                return Poll::Ready(Err(io::Error::other(s3_error!(
                    InvalidRequest,
                    "the checksum trailer is missing"
                ))));
            };
            if algo.value(&computed) != Some(claimed.trim()) {
                return Poll::Ready(Err(mismatch(algo)));
            }
            *this.outcome.lock().expect("checksum lock") = Some(computed);
        }
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;

    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[tokio::test]
    async fn verifies_header_checksums() {
        // CRC32 of "hello" is 0x3610a686.
        let good = headers(&[("x-amz-checksum-crc32", "NhCmhg==")]);
        let exp = expected(&good, None).unwrap();
        let (mut r, outcome) = wrap(&b"hello"[..], exp);
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.unwrap();
        assert_eq!(
            outcome
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .checksum_crc32
                .as_deref(),
            Some("NhCmhg==")
        );

        let bad = headers(&[("x-amz-checksum-crc32", "AAAAAA==")]);
        let (mut r, _) = wrap(&b"hello"[..], expected(&bad, None).unwrap());
        let err = r.read_to_end(&mut Vec::new()).await.unwrap_err();
        let inner = err.into_inner().unwrap().downcast::<S3Error>().unwrap();
        assert_eq!(inner.code(), &s3s::S3ErrorCode::BadDigest);
    }

    #[test]
    fn rejects_conflicting_or_unknown_checksums() {
        let two = headers(&[
            ("x-amz-checksum-crc32", "a"),
            ("x-amz-checksum-sha256", "b"),
        ]);
        assert!(expected(&two, None).is_err());
        let unknown = headers(&[("x-amz-trailer", "x-amz-checksum-whirlpool")]);
        assert!(expected(&unknown, None).is_err());
        assert!(expected(&headers(&[]), None).unwrap().is_none());
    }
}
