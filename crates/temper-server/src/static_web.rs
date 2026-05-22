//! Optional static web surfaces served by the Temper binary.

use std::path::{Component, Path, PathBuf};

use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{StatusCode, Uri};
use axum::response::Response;

const GENESIS_WEB_DIR_ENV: &str = "TEMPER_GENESIS_WEB_DIR";

/// Serve the Genesis registry SPA from `TEMPER_GENESIS_WEB_DIR`.
///
/// Unknown non-file paths fall back to `index.html` so the frontend can own
/// client-side routing. The route is disabled unless the env var is set.
pub(crate) async fn serve_genesis_web(uri: Uri) -> Response {
    let Some(root) = std::env::var_os(GENESIS_WEB_DIR_ENV) else {
        return not_found("Genesis web build is not configured");
    };
    let root = PathBuf::from(root);
    let Some(requested) = requested_asset_path(&root, uri.path()) else {
        return not_found("Genesis web asset not found");
    };

    let file_path = if tokio::fs::metadata(&requested)
        .await
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
    {
        requested
    } else {
        root.join("index.html")
    };

    match tokio::fs::read(&file_path).await {
        Ok(bytes) => static_response(&file_path, bytes),
        Err(_) => not_found("Genesis web asset not found"),
    }
}

fn requested_asset_path(root: &Path, uri_path: &str) -> Option<PathBuf> {
    let relative = uri_path
        .strip_prefix("/genesis")
        .unwrap_or(uri_path)
        .trim_start_matches('/');
    if relative.is_empty() {
        return Some(root.join("index.html"));
    }

    let mut path = root.to_path_buf();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => path.push(part),
            _ => return None,
        }
    }
    Some(path)
}

fn static_response(path: &Path, bytes: Vec<u8>) -> Response {
    let cache_control = if path.file_name().and_then(|name| name.to_str()) == Some("index.html") {
        "no-cache"
    } else {
        "public, max-age=3600"
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, content_type(path))
        .header(CACHE_CONTROL, cache_control)
        .body(Body::from(bytes))
        .expect("valid static web response") // ci-ok: static headers are valid
}

fn not_found(message: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(message))
        .expect("valid not-found response") // ci-ok: static headers are valid
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("css") => "text/css; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
