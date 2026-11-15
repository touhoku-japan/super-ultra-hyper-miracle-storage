//! AWS Signature Version 4 signing, for requests the Web console makes on a user's behalf: the
//! console proxy signs each browser request with the user's own key and hands it to the S3
//! service in-process, and share links are presigned URLs.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// Characters left as-is by S3's canonical URI encoding.
fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
}

/// Percent-encodes `s` the way SigV4 canonicalizes it (`/` encoded too).
pub fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if unreserved(b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-encodes a path, keeping the `/` separators.
pub fn path_encode(s: &str) -> String {
    s.split('/').map(uri_encode).collect::<Vec<_>>().join("/")
}

/// Canonical query string: encoded pairs sorted by key, then value.
pub fn canonical_query(pairs: &[(String, String)]) -> String {
    let mut q: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (uri_encode(k), uri_encode(v)))
        .collect();
    q.sort();
    q.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// `(yyyymmdd, yyyymmddThhmmssZ)` for a unix time.
pub fn amz_date(unix_secs: i64) -> (String, String) {
    let t = time::OffsetDateTime::from_unix_timestamp(unix_secs)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let date = format!("{:04}{:02}{:02}", t.year(), u8::from(t.month()), t.day());
    let stamp = format!("{date}T{:02}{:02}{:02}Z", t.hour(), t.minute(), t.second());
    (date, stamp)
}

pub fn now_secs() -> i64 {
    crate::admin::now_millis() / 1000
}

pub struct Credentials<'a> {
    pub access_key: &'a str,
    pub secret: &'a str,
    pub region: &'a str,
}

impl Credentials<'_> {
    fn signing_key(&self, date: &str) -> Vec<u8> {
        let k = hmac(format!("AWS4{}", self.secret).as_bytes(), date);
        let k = hmac(&k, self.region);
        let k = hmac(&k, "s3");
        hmac(&k, "aws4_request")
    }

    fn scope(&self, date: &str) -> String {
        format!("{date}/{}/s3/aws4_request", self.region)
    }

    fn signature(&self, date: &str, stamp: &str, canonical_request: &str) -> String {
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{stamp}\n{}\n{}",
            self.scope(date),
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );
        hex::encode(hmac(&self.signing_key(date), &string_to_sign))
    }

    /// The `Authorization` header for a request. `encoded_path` and `query` must be exactly
    /// what the request line carries (see [`path_encode`] / [`canonical_query`]); `headers` are
    /// the signed headers (names lowercase), and must include `host`, `x-amz-date` (`stamp`) and
    /// `x-amz-content-sha256` (`payload_hash`).
    pub fn authorization(
        &self,
        method: &str,
        encoded_path: &str,
        query: &str,
        headers: &[(String, String)],
        payload_hash: &str,
        unix_secs: i64,
    ) -> String {
        let (date, stamp) = amz_date(unix_secs);
        let mut h: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
            .collect();
        h.sort();
        let canonical_headers: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
        let signed = h
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let canonical_request = format!(
            "{method}\n{encoded_path}\n{query}\n{canonical_headers}\n{signed}\n{payload_hash}"
        );
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={signed}, Signature={}",
            self.access_key,
            self.scope(&date),
            self.signature(&date, &stamp, &canonical_request)
        )
    }

    /// A presigned GET URL for `<base_url><encoded_path>` valid for `expires_secs`.
    /// `host` is the Host the URL will be requested with.
    pub fn presign_get(
        &self,
        base_url: &str,
        host: &str,
        encoded_path: &str,
        extra_query: &[(String, String)],
        expires_secs: u64,
        unix_secs: i64,
    ) -> String {
        let (date, stamp) = amz_date(unix_secs);
        let mut pairs = extra_query.to_vec();
        pairs.extend([
            ("X-Amz-Algorithm".to_owned(), "AWS4-HMAC-SHA256".to_owned()),
            (
                "X-Amz-Credential".to_owned(),
                format!("{}/{}", self.access_key, self.scope(&date)),
            ),
            ("X-Amz-Date".to_owned(), stamp.clone()),
            ("X-Amz-Expires".to_owned(), expires_secs.to_string()),
            ("X-Amz-SignedHeaders".to_owned(), "host".to_owned()),
        ]);
        let query = canonical_query(&pairs);
        let canonical_request =
            format!("GET\n{encoded_path}\n{query}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD");
        let signature = self.signature(&date, &stamp, &canonical_request);
        format!(
            "{}{encoded_path}?{query}&X-Amz-Signature={signature}",
            base_url.trim_end_matches('/')
        )
    }
}

/// The host (with port, as sent in a Host header) of an `http(s)://host[:port][/...]` URL.
pub fn url_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split('/').next()?;
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_s3() {
        assert_eq!(uri_encode("a b/c~*"), "a%20b%2Fc~%2A");
        assert_eq!(path_encode("/bkt/a b/日"), "/bkt/a%20b/%E6%97%A5");
        assert_eq!(
            canonical_query(&[("b".into(), "2".into()), ("a".into(), "".into())]),
            "a=&b=2"
        );
    }

    /// The example from the AWS documentation ("Authenticating Requests: Using Query
    /// Parameters"): presigned GET of /test.txt valid for 86400 seconds.
    #[test]
    fn presign_matches_aws_example() {
        let c = Credentials {
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
        };
        // 20130524T000000Z
        let url = c.presign_get(
            "https://examplebucket.s3.amazonaws.com",
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            &[],
            86400,
            1_369_353_600,
        );
        assert!(
            url.ends_with(
                "X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
            ),
            "{url}"
        );
    }

    #[test]
    fn hosts_of_urls() {
        assert_eq!(url_host("https://s3.example.com/"), Some("s3.example.com"));
        assert_eq!(url_host("http://10.0.0.1:9000"), Some("10.0.0.1:9000"));
        assert_eq!(url_host("ftp://x"), None);
    }
}
