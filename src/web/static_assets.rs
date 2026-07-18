//! Static assets (CSS, icons, vendor JS) embedded into the binary at compile
//! time via `include_bytes!`, so the server is fully standalone: no files on
//! disk are needed at runtime. Each route serves its asset from memory with the
//! matching content type; the versioned vendor files additionally get a
//! long-immutable cache-control header (their filenames carry the version, so a
//! cached copy can never go stale).

use axum::body::Body;
use axum::extract::Path;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

// Embedded at compile time. Paths are relative to this file (src/web/), so
// ../../static/ reaches the repo static dir from here.
const HTMX_JS: &[u8] = include_bytes!("../../static/vendor/htmx-2.0.4.js");
const HTMX_SSE_JS: &[u8] = include_bytes!("../../static/vendor/htmx-ext-sse-2.2.4.js");
const APP_CSS: &[u8] = include_bytes!("../../static/css/app.css");
const BATTERY_LOW_SVG: &[u8] = include_bytes!("../../static/icons/battery-low.svg");

/// Build a 200 response for an embedded asset. `immutable` adds the
/// long-immutable cache-control header used by the versioned vendor files.
fn serve(data: &'static [u8], mime: &'static str, immutable: bool) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    if immutable {
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
    }
    (StatusCode::OK, headers, Body::from(data)).into_response()
}

/// `GET /vendor/{file}` -- versioned vendor JS (htmx + sse extension), cached
/// long-immutable.
pub async fn vendor(Path(file): Path<String>) -> Response {
    match file.as_str() {
        "htmx-2.0.4.js" => serve(HTMX_JS, "text/javascript; charset=utf-8", true),
        "htmx-ext-sse-2.2.4.js" => serve(HTMX_SSE_JS, "text/javascript; charset=utf-8", true),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `GET /css/{file}` -- the site stylesheet, embedded in the binary.
pub async fn css(Path(file): Path<String>) -> Response {
    match file.as_str() {
        "app.css" => serve(APP_CSS, "text/css; charset=utf-8", false),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `GET /icons/{file}` -- icon assets (e.g. the low-battery sensor indicator).
pub async fn icons(Path(file): Path<String>) -> Response {
    match file.as_str() {
        "battery-low.svg" => serve(BATTERY_LOW_SVG, "image/svg+xml", false),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}
