//! Web console sessions: stateless, HMAC-signed cookies.
//!
//! A session names its subject (an access key id, or the operator) and an expiry. Nothing is
//! stored server-side, so any node that knows the signing key accepts it; the subject is
//! re-checked on every request (a disabled key or suspended tenant stops working within the
//! directory cache TTL). Logging out clears the cookie; a copied cookie stays valid until it
//! expires, so lifetimes are kept short.
//!
//! Cookies are `HttpOnly` and `SameSite=Strict`, and requests that change anything must also
//! carry the [`CSRF_HEADER`] (which a cross-site form cannot set).

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::http::{HeaderMap, HeaderValue, Method, header};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// Header that state-changing console requests must carry (any value).
pub const CSRF_HEADER: &str = "x-suhms-console";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// Subject: an access key id, or `"operator"`.
    pub sub: String,
    /// Expiry, unix seconds.
    pub exp: i64,
}

pub struct Sessions {
    key: [u8; 32],
    pub cookie_name: &'static str,
    pub ttl: Duration,
    pub secure: bool,
    /// Cookie `Path`.
    pub path: &'static str,
}

impl Sessions {
    pub fn new(
        key: [u8; 32],
        cookie_name: &'static str,
        path: &'static str,
        ttl: Duration,
        secure: bool,
    ) -> Self {
        Self {
            key,
            cookie_name,
            ttl,
            secure,
            path,
        }
    }

    fn mac(&self, data: &[u8]) -> Vec<u8> {
        let mut m = Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC key");
        m.update(data);
        m.finalize().into_bytes().to_vec()
    }

    /// A signed token for `sub`, valid for the session TTL from `now` (unix seconds).
    pub fn issue(&self, sub: &str, now: i64) -> String {
        let claims = Claims {
            sub: sub.to_owned(),
            exp: now + i64::try_from(self.ttl.as_secs()).unwrap_or(i64::MAX / 2),
        };
        let payload = B64.encode(serde_json::to_vec(&claims).expect("claims serialize"));
        let sig = B64.encode(self.mac(payload.as_bytes()));
        format!("{payload}.{sig}")
    }

    /// The claims of a valid, unexpired token.
    pub fn verify(&self, token: &str, now: i64) -> Option<Claims> {
        let (payload, sig) = token.split_once('.')?;
        let sig = B64.decode(sig).ok()?;
        let mut m = Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC key");
        m.update(payload.as_bytes());
        m.verify_slice(&sig).ok()?;
        let claims: Claims = serde_json::from_slice(&B64.decode(payload).ok()?).ok()?;
        (claims.exp > now).then_some(claims)
    }

    /// The session cookie of a request, verified.
    pub fn from_headers(&self, headers: &HeaderMap, now: i64) -> Option<Claims> {
        cookie(headers, self.cookie_name).and_then(|t| self.verify(&t, now))
    }

    pub fn set_cookie(&self, token: &str) -> HeaderValue {
        self.cookie_header(token, self.ttl.as_secs())
    }

    pub fn clear_cookie(&self) -> HeaderValue {
        self.cookie_header("", 0)
    }

    fn cookie_header(&self, value: &str, max_age: u64) -> HeaderValue {
        // No Domain attribute: the cookie is host-only, never sent to sibling subdomains
        // (such as the S3 endpoint that serves customer content).
        let secure = if self.secure { "; Secure" } else { "" };
        HeaderValue::from_str(&format!(
            "{}={value}; Path={}; Max-Age={max_age}; HttpOnly; SameSite=Strict{secure}",
            self.cookie_name, self.path
        ))
        .expect("cookie value is ASCII")
    }
}

/// The value of cookie `name`.
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == name).then(|| v.to_owned())
        })
}

/// Whether a cookie-authenticated request may proceed: reads always, writes only with the
/// [`CSRF_HEADER`].
pub fn csrf_ok(method: &Method, headers: &HeaderMap) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
        || headers.contains_key(CSRF_HEADER)
}

/// Security headers for console pages and API responses.
pub fn security_headers(headers: &mut HeaderMap) {
    let set = |h: &mut HeaderMap, k: &'static str, v: &'static str| {
        h.insert(k, HeaderValue::from_static(v));
    };
    set(
        headers,
        "content-security-policy",
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
         connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
    );
    set(headers, "x-content-type-options", "nosniff");
    set(headers, "x-frame-options", "DENY");
    set(headers, "referrer-policy", "no-referrer");
    set(headers, "cross-origin-opener-policy", "same-origin");
}

/// Login attempt limiter: failed attempts per client address and per account, so a password
/// (secret) cannot be guessed at speed. Successful logins do not count.
pub struct LoginLimiter {
    max_failures: u32,
    window: Duration,
    failures: Mutex<HashMap<String, (u32, Instant)>>,
}

impl Default for LoginLimiter {
    fn default() -> Self {
        Self::new(10, Duration::from_secs(300))
    }
}

impl LoginLimiter {
    pub fn new(max_failures: u32, window: Duration) -> Self {
        Self {
            max_failures,
            window,
            failures: Mutex::default(),
        }
    }

    /// Whether every key may try again.
    pub fn allowed(&self, keys: &[&str]) -> bool {
        let mut f = self.failures.lock().expect("login limiter lock");
        f.retain(|_, (_, at)| at.elapsed() < self.window);
        keys.iter()
            .all(|k| f.get(*k).is_none_or(|(n, _)| *n < self.max_failures))
    }

    pub fn failed(&self, keys: &[&str]) {
        let mut f = self.failures.lock().expect("login limiter lock");
        for k in keys {
            let e = f.entry((*k).to_owned()).or_insert((0, Instant::now()));
            e.0 += 1;
        }
    }
}

/// The client address of a request: the last `X-Forwarded-For` entry when the listener sits
/// behind a trusted reverse proxy (nginx appends the address it saw), else the peer.
pub fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>, trust_proxy: bool) -> String {
    if trust_proxy
        && let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
    {
        return ip.to_string();
    }
    peer.map_or_else(|| "unknown".to_owned(), |p| p.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions() -> Sessions {
        Sessions::new([7; 32], "s", "/", Duration::from_secs(60), true)
    }

    #[test]
    fn tokens_round_trip_and_expire() {
        let s = sessions();
        let t = s.issue("AKX", 1000);
        assert_eq!(s.verify(&t, 1000).unwrap().sub, "AKX");
        assert!(s.verify(&t, 1060).is_none());
    }

    #[test]
    fn tampered_or_foreign_tokens_are_rejected() {
        let s = sessions();
        let t = s.issue("AKX", 1000);
        let (payload, sig) = t.split_once('.').unwrap();
        let forged = B64.encode(br#"{"sub":"AKY","exp":99999}"#);
        assert!(s.verify(&format!("{forged}.{sig}"), 1000).is_none());
        assert!(s.verify(payload, 1000).is_none());
        let other = Sessions::new([8; 32], "s", "/", Duration::from_secs(60), true);
        assert!(other.verify(&t, 1000).is_none());
    }

    #[test]
    fn cookies_are_parsed_and_set_safely() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("a=1; s=tok; b=2"));
        assert_eq!(cookie(&h, "s").as_deref(), Some("tok"));
        assert_eq!(cookie(&h, "x"), None);
        let c = sessions().set_cookie("tok");
        let c = c.to_str().unwrap();
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Strict") && c.contains("Secure"));
        assert!(!c.contains("Domain"));
    }

    #[test]
    fn writes_need_the_csrf_header() {
        let mut h = HeaderMap::new();
        assert!(csrf_ok(&Method::GET, &h));
        assert!(!csrf_ok(&Method::PUT, &h));
        h.insert(CSRF_HEADER, HeaderValue::from_static("1"));
        assert!(csrf_ok(&Method::DELETE, &h));
    }

    #[test]
    fn login_limiter_blocks_after_failures() {
        let l = LoginLimiter::new(2, Duration::from_secs(60));
        assert!(l.allowed(&["ip", "key"]));
        l.failed(&["ip", "key"]);
        l.failed(&["ip"]);
        assert!(!l.allowed(&["ip"]));
        assert!(l.allowed(&["key"]));
    }

    #[test]
    fn client_ip_uses_the_proxy_header_only_when_trusted() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.2.3.4, 5.6.7.8"),
        );
        let peer = Some(SocketAddr::from(([127, 0, 0, 1], 1)));
        assert_eq!(client_ip(&h, peer, true), "5.6.7.8");
        assert_eq!(client_ip(&h, peer, false), "127.0.0.1");
    }
}
