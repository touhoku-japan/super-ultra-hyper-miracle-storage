//! `x-amz-checksum-*`: verification on upload, storage, and multipart checksums.
//!
//! The expected value comes either from a request header or, for aws-chunked uploads, from a
//! trailer named by `x-amz-trailer` (what current AWS SDKs send by default). The body is hashed
//! while it streams into the store; at end of stream the reader compares and, on mismatch,
//! fails the read so the store discards the write before anything is committed. The computed
//! checksum is then published to the store (as [`Stored`] JSON), which keeps it with the object
//! or part so GET/HEAD/ListParts can return it.
//!
//! Multipart uploads choose an algorithm and type at creation. Completing computes the object
//! checksum from the parts' checksums: `COMPOSITE` = hash of the concatenated binary part
//! checksums plus `-<parts>`; `FULL_OBJECT` (CRCs only) = the CRC of the whole object, combined
//! from the part CRCs without reading the data again.

use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, ready},
};

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use crc_fast::CrcAlgorithm;
use s3s::{
    S3Error, S3Result,
    checksum::ChecksumHasher,
    crypto::{
        Checksum as _, Crc32, Crc32c, Crc64Nvme, Md5, Sha1, Sha256, Sha512, XxHash3, XxHash64,
        XxHash128,
    },
    dto::{Checksum, ChecksumAlgorithm, ChecksumType},
    s3_error,
};
use serde::{Deserialize, Serialize};
use suhms_store::{ChecksumSlot, PartRecord, StoreError, UploadRecord};
use tokio::io::{AsyncRead, ReadBuf};

pub const FULL_OBJECT: &str = ChecksumType::FULL_OBJECT;
pub const COMPOSITE: &str = ChecksumType::COMPOSITE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algo {
    Crc32,
    Crc32c,
    Crc64Nvme,
    Sha1,
    Sha256,
    Sha512,
    Md5,
    XxHash64,
    XxHash3,
    XxHash128,
}

/// What uploads without a checksum get, as in S3 (and what multipart uploads without an
/// algorithm combine into a full-object checksum).
pub const DEFAULT_ALGO: Algo = Algo::Crc64Nvme;

const ALGOS: [(Algo, &str); 10] = [
    (Algo::Crc32, "x-amz-checksum-crc32"),
    (Algo::Crc32c, "x-amz-checksum-crc32c"),
    (Algo::Crc64Nvme, "x-amz-checksum-crc64nvme"),
    (Algo::Sha1, "x-amz-checksum-sha1"),
    (Algo::Sha256, "x-amz-checksum-sha256"),
    (Algo::Sha512, "x-amz-checksum-sha512"),
    (Algo::Md5, "x-amz-checksum-md5"),
    (Algo::XxHash64, "x-amz-checksum-xxhash64"),
    (Algo::XxHash3, "x-amz-checksum-xxhash3"),
    (Algo::XxHash128, "x-amz-checksum-xxhash128"),
];

impl Algo {
    pub fn name(self) -> &'static str {
        match self {
            Self::Crc32 => ChecksumAlgorithm::CRC32,
            Self::Crc32c => ChecksumAlgorithm::CRC32C,
            Self::Crc64Nvme => ChecksumAlgorithm::CRC64NVME,
            Self::Sha1 => ChecksumAlgorithm::SHA1,
            Self::Sha256 => ChecksumAlgorithm::SHA256,
            Self::Sha512 => ChecksumAlgorithm::SHA512,
            Self::Md5 => ChecksumAlgorithm::MD5,
            Self::XxHash64 => ChecksumAlgorithm::XXHASH64,
            Self::XxHash3 => ChecksumAlgorithm::XXHASH3,
            Self::XxHash128 => ChecksumAlgorithm::XXHASH128,
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        ALGOS
            .iter()
            .map(|(a, _)| *a)
            .find(|a| a.name().eq_ignore_ascii_case(name))
    }

    /// The algorithm a request names (`x-amz-checksum-algorithm` and friends).
    pub fn of(a: Option<&ChecksumAlgorithm>) -> S3Result<Option<Self>> {
        a.map(|a| {
            Self::parse(a.as_str()).ok_or_else(|| {
                s3_error!(
                    InvalidRequest,
                    "unsupported checksum algorithm {}",
                    a.as_str()
                )
            })
        })
        .transpose()
    }

    fn crc(self) -> Option<CrcAlgorithm> {
        match self {
            Self::Crc32 => Some(CrcAlgorithm::Crc32IsoHdlc),
            Self::Crc32c => Some(CrcAlgorithm::Crc32Iscsi),
            Self::Crc64Nvme => Some(CrcAlgorithm::Crc64Nvme),
            _ => None,
        }
    }

    /// Default multipart checksum type: CRC64NVME only supports full-object checksums.
    pub fn default_type(self) -> &'static str {
        if self == Self::Crc64Nvme {
            FULL_OBJECT
        } else {
            COMPOSITE
        }
    }

    /// FULL_OBJECT needs a CRC (combinable without the data); COMPOSITE works for any other
    /// algorithm, CRC64NVME being full-object only.
    fn supports(self, kind: &str) -> bool {
        match kind {
            FULL_OBJECT => self.crc().is_some(),
            COMPOSITE => self != Self::Crc64Nvme,
            _ => false,
        }
    }

    fn enable(self, h: &mut ChecksumHasher) {
        match self {
            Self::Crc32 => h.crc32 = Some(Crc32::new()),
            Self::Crc32c => h.crc32c = Some(Crc32c::new()),
            Self::Crc64Nvme => h.crc64nvme = Some(Crc64Nvme::new()),
            Self::Sha1 => h.sha1 = Some(Sha1::new()),
            Self::Sha256 => h.sha256 = Some(Sha256::new()),
            Self::Sha512 => h.sha512 = Some(Sha512::new()),
            Self::Md5 => h.md5 = Some(Md5::new()),
            Self::XxHash64 => h.xxhash64 = Some(XxHash64::new()),
            Self::XxHash3 => h.xxhash3 = Some(XxHash3::new()),
            Self::XxHash128 => h.xxhash128 = Some(XxHash128::new()),
        }
    }

    fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Crc32 => Crc32::checksum(data).as_ref().to_vec(),
            Self::Crc32c => Crc32c::checksum(data).as_ref().to_vec(),
            Self::Crc64Nvme => Crc64Nvme::checksum(data).as_ref().to_vec(),
            Self::Sha1 => Sha1::checksum(data).as_ref().to_vec(),
            Self::Sha256 => Sha256::checksum(data).as_ref().to_vec(),
            Self::Sha512 => Sha512::checksum(data).as_ref().to_vec(),
            Self::Md5 => Md5::checksum(data).as_ref().to_vec(),
            Self::XxHash64 => XxHash64::checksum(data).as_ref().to_vec(),
            Self::XxHash3 => XxHash3::checksum(data).as_ref().to_vec(),
            Self::XxHash128 => XxHash128::checksum(data).as_ref().to_vec(),
        }
    }

    fn value(self, c: &Checksum) -> Option<&str> {
        match self {
            Self::Crc32 => c.checksum_crc32.as_deref(),
            Self::Crc32c => c.checksum_crc32c.as_deref(),
            Self::Crc64Nvme => c.checksum_crc64nvme.as_deref(),
            Self::Sha1 => c.checksum_sha1.as_deref(),
            Self::Sha256 => c.checksum_sha256.as_deref(),
            Self::Sha512 => c.checksum_sha512.as_deref(),
            Self::Md5 => c.checksum_md5.as_deref(),
            Self::XxHash64 => c.checksum_xxhash64.as_deref(),
            Self::XxHash3 => c.checksum_xxhash3.as_deref(),
            Self::XxHash128 => c.checksum_xxhash128.as_deref(),
        }
    }

    fn set(self, c: &mut Checksum, v: String) {
        let field = match self {
            Self::Crc32 => &mut c.checksum_crc32,
            Self::Crc32c => &mut c.checksum_crc32c,
            Self::Crc64Nvme => &mut c.checksum_crc64nvme,
            Self::Sha1 => &mut c.checksum_sha1,
            Self::Sha256 => &mut c.checksum_sha256,
            Self::Sha512 => &mut c.checksum_sha512,
            Self::Md5 => &mut c.checksum_md5,
            Self::XxHash64 => &mut c.checksum_xxhash64,
            Self::XxHash3 => &mut c.checksum_xxhash3,
            Self::XxHash128 => &mut c.checksum_xxhash128,
        };
        *field = Some(v);
    }
}

/// Which checksum a body gets stored with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// The context requires this algorithm (a multipart upload's); a client checksum of
    /// another algorithm is an error.
    Required(Algo),
    /// The client's algorithm if it sent a checksum, otherwise [`DEFAULT_ALGO`].
    ClientOrDefault,
    /// Always store this algorithm; a client checksum of another algorithm is verified too.
    Always(Algo),
}

/// A checksum as kept by the store (JSON in the `checksum` column).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stored {
    pub algorithm: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// Base64, as on the wire (composite values end in `-<parts>`).
    pub value: String,
}

impl Stored {
    pub fn decode(s: Option<&str>) -> Option<Self> {
        s.and_then(|s| serde_json::from_str(s).ok())
    }

    fn encode(&self) -> String {
        serde_json::to_string(self).expect("checksum serializes")
    }

    pub fn algo(&self) -> Option<Algo> {
        Algo::parse(&self.algorithm)
    }

    /// The value in the response field of its algorithm.
    pub fn fields(&self) -> Checksum {
        let mut c = Checksum::default();
        if let Some(a) = self.algo() {
            a.set(&mut c, self.value.clone());
        }
        c
    }

    pub fn checksum_type(&self) -> ChecksumType {
        ChecksumType::from(self.kind.clone())
    }
}

/// A multipart upload's checksum settings (JSON in the upload's `checksum` column).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadChecksum {
    pub algorithm: String,
    #[serde(rename = "type")]
    pub kind: String,
}

impl UploadChecksum {
    /// Validates CreateMultipartUpload's `x-amz-checksum-algorithm` / `-type`.
    pub fn new(algo: Option<Algo>, kind: Option<&ChecksumType>) -> S3Result<Option<Self>> {
        let Some(algo) = algo else {
            if kind.is_some() {
                return Err(s3_error!(
                    InvalidRequest,
                    "x-amz-checksum-type requires x-amz-checksum-algorithm"
                ));
            }
            return Ok(None);
        };
        let kind = kind.map_or(algo.default_type(), ChecksumType::as_str);
        if !algo.supports(kind) {
            return Err(s3_error!(
                InvalidRequest,
                "checksum type {kind} is not supported for {}",
                algo.name()
            ));
        }
        Ok(Some(Self {
            algorithm: algo.name().to_owned(),
            kind: kind.to_owned(),
        }))
    }

    pub fn decode(s: Option<&str>) -> Option<Self> {
        s.and_then(|s| serde_json::from_str(s).ok())
    }

    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("checksum serializes")
    }

    pub fn algo(&self) -> Option<Algo> {
        Algo::parse(&self.algorithm)
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

impl Expected {
    pub fn algo(&self) -> Algo {
        self.algo
    }
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

/// Where the computed checksum is published (for the response) once the body has been read
/// completely.
pub type Outcome = Arc<Mutex<Option<Checksum>>>;

pub struct VerifyingReader<R> {
    inner: R,
    /// The algorithm stored with the body.
    store: Algo,
    expected: Option<Expected>,
    hasher: Option<ChecksumHasher>,
    outcome: Outcome,
    slot: ChecksumSlot,
}

/// Wraps `inner` so that its checksum is computed (per `policy`) and, if the client sent one,
/// verified at end of stream. The stored checksum is published to the returned slot.
#[allow(clippy::type_complexity)]
pub fn wrap<R>(
    inner: R,
    expected: Option<Expected>,
    policy: Policy,
) -> S3Result<(VerifyingReader<R>, Outcome, ChecksumSlot)>
where
    R: AsyncRead + Unpin,
{
    let client = expected.as_ref().map(|e| e.algo);
    let store = match policy {
        Policy::Required(a) => {
            if let Some(c) = client
                && c != a
            {
                return Err(s3_error!(
                    InvalidRequest,
                    "Checksum Type mismatch occurred, expected checksum Type: {}, actual checksum Type: {}",
                    a.name().to_ascii_lowercase(),
                    c.name().to_ascii_lowercase()
                ));
            }
            a
        }
        Policy::ClientOrDefault => client.unwrap_or(DEFAULT_ALGO),
        Policy::Always(a) => a,
    };
    let mut hasher = ChecksumHasher::default();
    store.enable(&mut hasher);
    if let Some(c) = client {
        c.enable(&mut hasher);
    }
    let outcome = Outcome::default();
    let slot = ChecksumSlot::default();
    let reader = VerifyingReader {
        inner,
        store,
        hasher: Some(hasher),
        expected,
        outcome: Arc::clone(&outcome),
        slot: Arc::clone(&slot),
    };
    Ok((reader, outcome, slot))
}

fn mismatch(algo: Algo) -> io::Error {
    io::Error::other(S3Error::with_message(
        s3s::S3ErrorCode::BadDigest,
        format!(
            "The {} you specified did not match the calculated checksum.",
            algo.name()
        ),
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
            if let Some(expected) = &this.expected {
                let algo = expected.algo;
                let claimed = match &expected.source {
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
            }
            if let Some(v) = this.store.value(&computed) {
                let stored = Stored {
                    algorithm: this.store.name().to_owned(),
                    kind: FULL_OBJECT.to_owned(),
                    value: v.to_owned(),
                };
                *this.slot.lock().expect("checksum lock") = Some(stored.encode());
            }
            *this.outcome.lock().expect("checksum lock") = Some(computed);
        }
        Poll::Ready(Ok(()))
    }
}

fn decode_b64(v: &str) -> Option<Vec<u8>> {
    B64.decode(v).ok()
}

/// The completed object's checksum, from the upload's settings and its parts' checksums.
/// `claimed_parts` are the per-part checksums the client listed (by position in `parts`), and
/// `claimed` the object checksum the client sent, if any; both must match.
pub fn complete(
    upload: &UploadRecord,
    parts: &[&PartRecord],
    claimed_parts: &[Checksum],
    claimed: &Checksum,
) -> Result<Option<String>, StoreError> {
    // Without an algorithm, parts carry the default CRC64NVME and the object gets its
    // full-object CRC (as in S3); parts stored before that existed have none.
    let cfg = match UploadChecksum::decode(upload.checksum.as_deref()) {
        Some(cfg) => cfg,
        None => {
            let all_default = parts.iter().all(|p| {
                Stored::decode(p.checksum.as_deref()).and_then(|s| s.algo()) == Some(DEFAULT_ALGO)
            });
            if !all_default {
                return Ok(None);
            }
            UploadChecksum {
                algorithm: DEFAULT_ALGO.name().to_owned(),
                kind: FULL_OBJECT.to_owned(),
            }
        }
    };
    let algo = cfg
        .algo()
        .ok_or_else(|| StoreError::InvalidRequest("unknown checksum algorithm".into()))?;
    let mut raws = Vec::with_capacity(parts.len());
    for (i, p) in parts.iter().enumerate() {
        let stored = Stored::decode(p.checksum.as_deref())
            .filter(|s| s.algo() == Some(algo))
            .ok_or_else(|| {
                StoreError::InvalidPart(format!(
                    "part {} has no {} checksum",
                    p.part_number,
                    algo.name()
                ))
            })?;
        if let Some(c) = claimed_parts.get(i).and_then(|c| algo.value(c))
            && c.trim() != stored.value
        {
            return Err(StoreError::InvalidPart(format!(
                "checksum of part {} does not match",
                p.part_number
            )));
        }
        let raw = decode_b64(&stored.value)
            .ok_or_else(|| StoreError::InvalidPart(format!("part {}", p.part_number)))?;
        raws.push((raw, p.size));
    }
    let value = if cfg.kind == FULL_OBJECT {
        let crc = algo
            .crc()
            .ok_or_else(|| StoreError::InvalidRequest("invalid checksum type".into()))?;
        let as_u64 = |b: &[u8]| b.iter().fold(0u64, |acc, x| (acc << 8) | u64::from(*x));
        let width = raws.first().map_or(4, |(r, _)| r.len());
        let mut acc: Option<u64> = None;
        for (raw, size) in &raws {
            let v = as_u64(raw);
            acc = Some(match acc {
                None => v,
                Some(a) => crc_fast::checksum_combine(crc, a, v, *size),
            });
        }
        let bytes = acc.unwrap_or(0).to_be_bytes();
        B64.encode(&bytes[8 - width..])
    } else {
        let concat: Vec<u8> = raws.iter().flat_map(|(r, _)| r.iter().copied()).collect();
        format!("{}-{}", B64.encode(algo.digest(&concat)), parts.len())
    };
    if let Some(c) = algo.value(claimed) {
        let strip = |v: &str| v.trim().split('-').next().unwrap_or_default().to_owned();
        if strip(c) != strip(&value) {
            return Err(StoreError::BadDigest);
        }
    }
    Ok(Some(
        Stored {
            algorithm: algo.name().to_owned(),
            kind: cfg.kind,
            value,
        }
        .encode(),
    ))
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;
    use uuid::Uuid;

    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    fn stored(slot: &ChecksumSlot) -> Stored {
        Stored::decode(slot.lock().unwrap().as_deref()).unwrap()
    }

    #[tokio::test]
    async fn verifies_header_checksums() {
        // CRC32 of "hello" is 0x3610a686.
        let good = headers(&[("x-amz-checksum-crc32", "NhCmhg==")]);
        let exp = expected(&good, None).unwrap();
        let (mut r, outcome, slot) = wrap(&b"hello"[..], exp, Policy::ClientOrDefault).unwrap();
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
        let s = stored(&slot);
        assert_eq!(
            (s.algorithm.as_str(), s.value.as_str()),
            ("CRC32", "NhCmhg==")
        );
        assert_eq!(s.kind, FULL_OBJECT);

        let bad = headers(&[("x-amz-checksum-crc32", "AAAAAA==")]);
        let (mut r, _, _) = wrap(
            &b"hello"[..],
            expected(&bad, None).unwrap(),
            Policy::ClientOrDefault,
        )
        .unwrap();
        let err = r.read_to_end(&mut Vec::new()).await.unwrap_err();
        let inner = err.into_inner().unwrap().downcast::<S3Error>().unwrap();
        assert_eq!(inner.code(), &s3s::S3ErrorCode::BadDigest);
    }

    #[tokio::test]
    async fn policies_choose_the_stored_algorithm() {
        // Nothing from the client: the default (CRC64NVME), as in S3.
        let (mut r, _, slot) = wrap(&b"hello"[..], None, Policy::ClientOrDefault).unwrap();
        r.read_to_end(&mut Vec::new()).await.unwrap();
        assert_eq!(
            stored(&slot).value,
            B64.encode(Crc64Nvme::checksum(b"hello"))
        );

        let (mut r, _, slot) = wrap(&b"hello"[..], None, Policy::Required(Algo::Sha256)).unwrap();
        r.read_to_end(&mut Vec::new()).await.unwrap();
        let s = stored(&slot);
        assert_eq!(s.algorithm, "SHA256");
        assert_eq!(s.value, B64.encode(Sha256::checksum(b"hello")));

        // Required: a client checksum of another algorithm is rejected up front.
        let crc = headers(&[("x-amz-checksum-crc32", "NhCmhg==")]);
        assert!(
            wrap(
                &b""[..],
                expected(&crc, None).unwrap(),
                Policy::Required(Algo::Sha1)
            )
            .is_err()
        );
        // Always: the client's checksum is still verified, the other algorithm is stored.
        let (mut r, outcome, slot) = wrap(
            &b"hello"[..],
            expected(&crc, None).unwrap(),
            Policy::Always(Algo::Crc64Nvme),
        )
        .unwrap();
        r.read_to_end(&mut Vec::new()).await.unwrap();
        assert_eq!(stored(&slot).algorithm, "CRC64NVME");
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
        let (mut r, _, _) = wrap(
            &b"hello"[..],
            expected(&bad, None).unwrap(),
            Policy::Always(Algo::Crc64Nvme),
        )
        .unwrap();
        assert!(r.read_to_end(&mut Vec::new()).await.is_err());
    }

    #[tokio::test]
    async fn newer_algorithms_are_verified() {
        for (algo, header) in [
            (Algo::Md5, "x-amz-checksum-md5"),
            (Algo::Sha512, "x-amz-checksum-sha512"),
            (Algo::XxHash64, "x-amz-checksum-xxhash64"),
            (Algo::XxHash3, "x-amz-checksum-xxhash3"),
            (Algo::XxHash128, "x-amz-checksum-xxhash128"),
        ] {
            let good = B64.encode(algo.digest(b"data"));
            let h = headers(&[(header, &good)]);
            let (mut r, _, slot) = wrap(
                &b"data"[..],
                expected(&h, None).unwrap(),
                Policy::ClientOrDefault,
            )
            .unwrap();
            r.read_to_end(&mut Vec::new()).await.unwrap();
            assert_eq!(stored(&slot).value, good, "{algo:?}");
            let h = headers(&[(header, "AAAA")]);
            let (mut r, _, _) = wrap(
                &b"data"[..],
                expected(&h, None).unwrap(),
                Policy::ClientOrDefault,
            )
            .unwrap();
            assert!(r.read_to_end(&mut Vec::new()).await.is_err(), "{algo:?}");
        }
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

    fn part(n: u32, algo: Algo, data: &[u8]) -> PartRecord {
        PartRecord {
            upload_id: Uuid::nil(),
            part_number: n,
            blob_id: Uuid::nil(),
            size: data.len() as u64,
            etag: String::new(),
            blake3: String::new(),
            disks: vec![],
            created_at: 0,
            checksum: Some(
                Stored {
                    algorithm: algo.name().to_owned(),
                    kind: FULL_OBJECT.to_owned(),
                    value: B64.encode(algo.digest(data)),
                }
                .encode(),
            ),
        }
    }

    fn upload(algo: Algo, kind: &str) -> UploadRecord {
        UploadRecord {
            id: Uuid::nil(),
            bucket: String::new(),
            key: String::new(),
            content_type: None,
            storage_class: None,
            user_metadata: Default::default(),
            desired_replicas: 1,
            replicas_explicit: false,
            disks: vec![],
            created_at: 0,
            checksum: Some(
                UploadChecksum {
                    algorithm: algo.name().to_owned(),
                    kind: kind.to_owned(),
                }
                .encode(),
            ),
        }
    }

    #[test]
    fn full_object_crcs_combine_to_the_whole_object_crc() {
        let (a, b, c) = (&b"hello, "[..], &b"multipart "[..], &b"world"[..]);
        for algo in [Algo::Crc32, Algo::Crc32c, Algo::Crc64Nvme] {
            let parts = [part(1, algo, a), part(2, algo, b), part(3, algo, c)];
            let refs: Vec<&PartRecord> = parts.iter().collect();
            let out =
                complete(&upload(algo, FULL_OBJECT), &refs, &[], &Checksum::default()).unwrap();
            let stored = Stored::decode(out.as_deref()).unwrap();
            assert_eq!(
                stored.value,
                B64.encode(algo.digest(b"hello, multipart world")),
                "{algo:?}"
            );
        }
    }

    #[test]
    fn uploads_without_an_algorithm_get_a_full_object_crc64nvme() {
        let parts = [
            part(1, Algo::Crc64Nvme, b"ab"),
            part(2, Algo::Crc64Nvme, b"cd"),
        ];
        let refs: Vec<&PartRecord> = parts.iter().collect();
        let mut u = upload(Algo::Crc64Nvme, FULL_OBJECT);
        u.checksum = None;
        let out = complete(&u, &refs, &[], &Checksum::default()).unwrap();
        let s = Stored::decode(out.as_deref()).unwrap();
        assert_eq!(
            (s.algorithm.as_str(), s.kind.as_str()),
            ("CRC64NVME", FULL_OBJECT)
        );
        assert_eq!(s.value, B64.encode(Crc64Nvme::checksum(b"abcd")));
        // Parts from before default checksums existed: no object checksum.
        let mut old = parts.clone();
        old[0].checksum = None;
        let refs: Vec<&PartRecord> = old.iter().collect();
        assert_eq!(
            complete(&u, &refs, &[], &Checksum::default()).unwrap(),
            None
        );
    }

    #[test]
    fn composite_checksums_hash_the_part_checksums() {
        let parts = [part(1, Algo::Sha256, b"aa"), part(2, Algo::Sha256, b"bb")];
        let refs: Vec<&PartRecord> = parts.iter().collect();
        let out = complete(
            &upload(Algo::Sha256, COMPOSITE),
            &refs,
            &[],
            &Checksum::default(),
        )
        .unwrap();
        let mut concat = Sha256::checksum(b"aa").as_ref().to_vec();
        concat.extend_from_slice(Sha256::checksum(b"bb").as_ref());
        let expected = format!("{}-2", B64.encode(Sha256::checksum(&concat)));
        assert_eq!(Stored::decode(out.as_deref()).unwrap().value, expected);

        // A part checksum listed by the client must match what was stored.
        let wrong = Checksum {
            checksum_sha256: Some(B64.encode(Sha256::checksum(b"xx"))),
            ..Default::default()
        };
        assert!(matches!(
            complete(
                &upload(Algo::Sha256, COMPOSITE),
                &refs,
                &[wrong],
                &Checksum::default()
            ),
            Err(StoreError::InvalidPart(_))
        ));
    }

    #[test]
    fn multipart_settings_are_validated() {
        assert!(
            UploadChecksum::new(Some(Algo::Crc64Nvme), None)
                .unwrap()
                .unwrap()
                .kind
                == FULL_OBJECT
        );
        assert!(
            UploadChecksum::new(Some(Algo::Sha1), None)
                .unwrap()
                .unwrap()
                .kind
                == COMPOSITE
        );
        let full = ChecksumType::from_static(FULL_OBJECT);
        let composite = ChecksumType::from_static(COMPOSITE);
        assert!(UploadChecksum::new(Some(Algo::Sha256), Some(&full)).is_err());
        assert!(UploadChecksum::new(Some(Algo::Crc64Nvme), Some(&composite)).is_err());
        assert!(UploadChecksum::new(None, Some(&full)).is_err());
        assert!(UploadChecksum::new(Some(Algo::Crc32), Some(&full)).is_ok());
    }
}
