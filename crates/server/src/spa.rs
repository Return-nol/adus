//! Dashboard SvelteKit (`web/build`) yang di-embed ke binary.
//! Build release memuat file saat compile; build debug membacanya dari disk.

use axum::{
    http::{Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

use crate::error::ApiError;

/// `allow_missing`: `cargo build` tetap jalan sebelum `pnpm -C web build`.
#[derive(RustEmbed)]
#[folder = "../../web/build"]
#[allow_missing = true]
struct Assets;

/// Fallback router (hanya GET/HEAD; method lain 404 JSON): file statis jika ada,
/// `/api/*` dan `/webhooks/*` yang tak dikenal 404 JSON,
/// selain itu `index.html` supaya router sisi klien SvelteKit yang menangani path-nya.
pub async fn spa_handler(method: Method, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    // Only page loads get the SPA; e.g. a POST to an unknown path is a plain 404.
    if !matches!(method, Method::GET | Method::HEAD) || is_backend_path(path) {
        return ApiError::not_found().into_response();
    }
    if let Some(response) = asset(path) {
        return response;
    }
    asset("index.html").unwrap_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "dashboard is not built: run `pnpm -C web build` and rebuild the server",
        )
            .into_response()
    })
}

fn is_backend_path(path: &str) -> bool {
    ["api", "webhooks"].iter().any(|prefix| {
        path == *prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn asset(path: &str) -> Option<Response> {
    let file = Assets::get(path)?;
    let cache_control = if path == "index.html" {
        "no-cache"
    } else if path.starts_with("_app/immutable/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };
    Some(
        (
            [
                (header::CONTENT_TYPE, file.metadata.mimetype().to_string()),
                (header::CACHE_CONTROL, cache_control.to_string()),
            ],
            file.data,
        )
            .into_response(),
    )
}
