//! The web console: static files compiled into the binary. It talks to the
//! same `/mcp` tools Claude does, so roles and the audit log cover both.

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

include!(concat!(env!("OUT_DIR"), "/web_files.rs"));

const CSP: &str = "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; \
                   connect-src 'self' ws: wss:; frame-ancestors 'none'; base-uri 'none'";

fn file(name: &str) -> Option<&'static [u8]> {
    FILES.iter().find(|(n, _)| *n == name).map(|(_, b)| *b)
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("v1/") || path == "v1" || path.starts_with("mcp") {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some(bytes) = file(path).filter(|_| !path.is_empty()) {
        // Vite names assets after their contents.
        let cache = if path.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        return (
            [
                (header::CONTENT_TYPE, content_type(path)),
                (header::CACHE_CONTROL, cache),
            ],
            bytes,
        )
            .into_response();
    }
    // Anything else is a page of the console.
    match file("index.html") {
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
                (header::CONTENT_SECURITY_POLICY, CSP),
                (header::X_FRAME_OPTIONS, "DENY"),
            ],
            bytes,
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "This server was built without the web console: run `bun run build` in web/, \
             then rebuild. MCP is at /mcp.\n",
        )
            .into_response(),
    }
}
