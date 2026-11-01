//! Studio's static files, embedded in the binary.

use axum::http::{StatusCode, header};
use axum::response::IntoResponse;

#[derive(rust_embed::Embed)]
#[folder = "static/"]
struct StaticAssets;

// -- GET /* (static fallback) ----------------------------------------

pub(super) async fn static_handler(uri: axum::http::Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    // gzip variant first (e.g. "index.html.gz"), then uncompressed, then index.html for SPA routing
    let gz_path = format!("{path}.gz");
    if let Some(content) = StaticAssets::get(&gz_path) {
        let mime = guess_mime(path);
        return build_static_response(mime, Some("gzip"), content.data.to_vec());
    }
    if let Some(content) = StaticAssets::get(path) {
        let mime = guess_mime(path);
        return build_static_response(mime, None, content.data.to_vec());
    }
    // SPA fallback: index.html (maybe gzipped)
    if let Some(content) = StaticAssets::get("index.html.gz") {
        let mime = guess_mime("index.html");
        return build_static_response(mime, Some("gzip"), content.data.to_vec());
    }
    if let Some(content) = StaticAssets::get("index.html") {
        let mime = guess_mime("index.html");
        return build_static_response(mime, None, content.data.to_vec());
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// Static-asset response with the cross-origin-isolation headers Studio needs
/// for the threaded WASM build. `SharedArrayBuffer` (wasm-bindgen-rayon's
/// thread pool) only exists on cross-origin-isolated pages; the first two
/// headers are the required pair, the third
/// (`Cross-Origin-Resource-Policy: same-origin`) lets the same-origin page load
/// the WASM binary without a CORS preflight. See docs/reference/wasm-deployment.md.
fn build_static_response(
    mime: &'static str,
    encoding: Option<&'static str>,
    body: Vec<u8>,
) -> axum::response::Response {
    let mut builder = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header("Cross-Origin-Embedder-Policy", "require-corp")
        .header("Cross-Origin-Opener-Policy", "same-origin")
        .header("Cross-Origin-Resource-Policy", "same-origin");
    if let Some(enc) = encoding {
        builder = builder.header(header::CONTENT_ENCODING, enc);
    }
    builder.body(body.into()).unwrap_or_else(|_| {
        (StatusCode::INTERNAL_SERVER_ERROR, "header build failed").into_response()
    })
}

/// MIME type by file extension.
fn guess_mime(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") | Some("mjs") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        _ => "application/octet-stream",
    }
}
