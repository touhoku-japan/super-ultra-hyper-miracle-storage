//! Bucket CORS: rules set with PutBucketCors, applied by a middleware that answers browser
//! preflight requests (`OPTIONS`) and adds `Access-Control-*` headers to actual responses.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use s3s::dto::CORSRule;
use serde::{Deserialize, Serialize};
use suhms_store::Store;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorsRule {
    pub allowed_origins: Vec<String>,
    pub allowed_methods: Vec<String>,
    #[serde(default)]
    pub allowed_headers: Vec<String>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    pub max_age_seconds: Option<i32>,
}

impl From<CORSRule> for CorsRule {
    fn from(r: CORSRule) -> Self {
        Self {
            allowed_origins: r.allowed_origins,
            allowed_methods: r.allowed_methods,
            allowed_headers: r.allowed_headers.unwrap_or_default(),
            expose_headers: r.expose_headers.unwrap_or_default(),
            max_age_seconds: r.max_age_seconds,
        }
    }
}

impl From<CorsRule> for CORSRule {
    fn from(r: CorsRule) -> Self {
        Self {
            allowed_origins: r.allowed_origins,
            allowed_methods: r.allowed_methods,
            allowed_headers: (!r.allowed_headers.is_empty()).then_some(r.allowed_headers),
            expose_headers: (!r.expose_headers.is_empty()).then_some(r.expose_headers),
            max_age_seconds: r.max_age_seconds,
            id: None,
        }
    }
}

pub fn decode(s: Option<&str>) -> Option<Vec<CorsRule>> {
    s.and_then(|s| serde_json::from_str(s).ok())
}

/// `pattern` may contain one `*` matching any run of characters (as in S3).
fn wildcard_match(pattern: &str, value: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern.eq_ignore_ascii_case(value),
        Some((pre, post)) => {
            value.len() >= pre.len() + post.len()
                && value[..pre.len()].eq_ignore_ascii_case(pre)
                && value[value.len() - post.len()..].eq_ignore_ascii_case(post)
        }
    }
}

impl CorsRule {
    fn allows(&self, origin: &str, method: &str, headers: &[&str]) -> bool {
        self.allowed_origins
            .iter()
            .any(|o| wildcard_match(o, origin))
            && self
                .allowed_methods
                .iter()
                .any(|m| m.eq_ignore_ascii_case(method))
            && headers.iter().all(|h| {
                self.allowed_headers
                    .iter()
                    .any(|a| wildcard_match(a, h.trim()))
            })
    }
}

/// The first rule allowing the request, as S3 evaluates them.
pub fn matching<'a>(
    rules: &'a [CorsRule],
    origin: &str,
    method: &str,
    headers: &[&str],
) -> Option<&'a CorsRule> {
    rules.iter().find(|r| r.allows(origin, method, headers))
}

#[derive(Clone)]
pub struct CorsState {
    pub store: Arc<Store>,
    /// Base domains for virtual-hosted-style requests.
    pub domains: Arc<[String]>,
}

impl CorsState {
    fn bucket_of(&self, req: &Request<Body>) -> Option<String> {
        let host = req
            .headers()
            .get("host")
            .and_then(|h| h.to_str().ok())
            .map(|h| h.split(':').next().unwrap_or(h));
        if let Some(host) = host {
            for d in self.domains.iter() {
                if let Some(bucket) = host.strip_suffix(&format!(".{d}")) {
                    return Some(bucket.to_owned());
                }
            }
        }
        req.uri()
            .path()
            .trim_start_matches('/')
            .split('/')
            .next()
            .filter(|b| !b.is_empty())
            .map(str::to_owned)
    }

    async fn rules(&self, bucket: &str) -> Option<Vec<CorsRule>> {
        let b = self.store.metadata().get_bucket(bucket).await.ok()??;
        decode(b.cors.as_deref())
    }
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

fn set(h: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        h.insert(name, v);
    }
}

fn apply(h: &mut HeaderMap, rule: &CorsRule, origin: &str) {
    let any = rule.allowed_origins.iter().any(|o| o == "*");
    set(
        h,
        "access-control-allow-origin",
        if any { "*" } else { origin },
    );
    set(
        h,
        "access-control-allow-methods",
        &rule.allowed_methods.join(", "),
    );
    if !rule.expose_headers.is_empty() {
        set(
            h,
            "access-control-expose-headers",
            &rule.expose_headers.join(", "),
        );
    }
    if let Some(age) = rule.max_age_seconds {
        set(h, "access-control-max-age", &age.to_string());
    }
    if !any {
        set(h, "vary", "Origin");
    }
}

pub async fn middleware(State(st): State<CorsState>, req: Request<Body>, next: Next) -> Response {
    let Some(origin) = header(req.headers(), "origin").map(str::to_owned) else {
        return next.run(req).await;
    };
    let bucket = st.bucket_of(&req);

    if req.method() == Method::OPTIONS {
        let method = header(req.headers(), "access-control-request-method").unwrap_or_default();
        let requested = header(req.headers(), "access-control-request-headers")
            .unwrap_or_default()
            .to_owned();
        let headers: Vec<&str> = requested
            .split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .collect();
        let rules = match &bucket {
            Some(b) => st.rules(b).await,
            None => None,
        };
        let Some(rule) = rules
            .as_deref()
            .and_then(|r| matching(r, &origin, method, &headers))
        else {
            return (
                StatusCode::FORBIDDEN,
                "CORSResponse: This CORS request is not allowed.",
            )
                .into_response();
        };
        let mut resp = StatusCode::OK.into_response();
        apply(resp.headers_mut(), rule, &origin);
        if !requested.is_empty() {
            set(
                resp.headers_mut(),
                "access-control-allow-headers",
                &requested,
            );
        }
        return resp;
    }

    let method = req.method().as_str().to_owned();
    let mut resp = next.run(req).await;
    if let Some(b) = bucket
        && let Some(rules) = st.rules(&b).await
        && let Some(rule) = matching(&rules, &origin, &method, &[])
    {
        apply(resp.headers_mut(), rule, &origin);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(origins: &[&str], methods: &[&str], headers: &[&str]) -> CorsRule {
        CorsRule {
            allowed_origins: origins.iter().map(|s| (*s).to_owned()).collect(),
            allowed_methods: methods.iter().map(|s| (*s).to_owned()).collect(),
            allowed_headers: headers.iter().map(|s| (*s).to_owned()).collect(),
            ..CorsRule::default()
        }
    }

    #[test]
    fn origins_methods_and_headers_must_all_match() {
        let rules = [rule(
            &["https://*.sorahost.net"],
            &["GET", "PUT"],
            &["content-*"],
        )];
        assert!(matching(&rules, "https://app.sorahost.net", "GET", &[]).is_some());
        assert!(matching(&rules, "https://app.sorahost.net", "PUT", &["Content-Type"]).is_some());
        assert!(matching(&rules, "https://evil.example", "GET", &[]).is_none());
        assert!(matching(&rules, "https://app.sorahost.net", "DELETE", &[]).is_none());
        assert!(matching(&rules, "https://app.sorahost.net", "PUT", &["x-amz-acl"]).is_none());
        assert!(matching(&[rule(&["*"], &["GET"], &[])], "http://x", "GET", &[]).is_some());
    }
}
