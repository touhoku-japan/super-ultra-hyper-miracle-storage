//! The Web console's static files (the Svelte app in `web/`, embedded at build time by
//! `build.rs`). The same app serves both consoles; it asks `api/session` which one it is.

use axum::{
    body::Body,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));
}

const PLACEHOLDER: &str = "<!doctype html><html lang=\"ja\"><meta charset=\"utf-8\">\
<title>suhms</title><p>Web コンソールがビルドされていません（<code>web/</code> で \
<code>npm run build</code> を実行してから suhms をビルドしてください）。</p></html>";

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" | "map" => "application/json",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn find(name: &str) -> Option<&'static [u8]> {
    embedded::ASSETS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, data)| *data)
}

/// Whether the console was built into this binary.
pub fn is_built() -> bool {
    find("index.html").is_some()
}

/// Serves `path` (relative to the console root). Unknown paths get `index.html`, except under
/// `assets/`, so a typo in a script URL is a 404 rather than HTML.
pub fn serve(path: &str) -> Response {
    let path = path.trim_start_matches('/');
    let (name, data) = match find(path) {
        Some(data) if !path.is_empty() => (path, data),
        _ if path.starts_with("assets/") => return StatusCode::NOT_FOUND.into_response(),
        _ => match find("index.html") {
            Some(data) => ("index.html", data),
            None => ("index.html", PLACEHOLDER.as_bytes()),
        },
    };
    let mut resp = Response::new(Body::from(data));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(name)),
    );
    // Vite puts a content hash in every file name under assets/.
    let cache = if name.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    crate::session::security_headers(h);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_index_for_unknown_paths_but_not_for_assets() {
        let r = serve("/");
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            r.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert!(r.headers().contains_key("content-security-policy"));
        assert_eq!(serve("/buckets/x").status(), StatusCode::OK);
        assert_eq!(serve("/assets/missing.js").status(), StatusCode::NOT_FOUND);
    }
}
