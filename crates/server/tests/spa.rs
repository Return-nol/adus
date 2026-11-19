//! Tes ini membaca `web/build`; jalankan `pnpm -C web build` dulu.
mod common;

use std::path::{Path, PathBuf};

use axum::http::header;
use common::{app, get, send};
use http_body_util::BodyExt;
use sqlx::PgPool;
use tower::ServiceExt;

fn build_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/build");
    assert!(
        dir.join("index.html").exists(),
        "frontend belum dibuild: jalankan `pnpm -C web build` lalu ulangi tes ini"
    );
    dir
}

/// File `.js` pertama di bawah `dir` (path relatif terhadap build, pakai `/`).
fn first_js(dir: &Path, root: &Path) -> Option<String> {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if let Some(found) = first_js(&path, root) {
                return Some(found);
            }
        } else if path.extension().is_some_and(|ext| ext == "js") {
            let relative = path.strip_prefix(root).unwrap();
            return Some(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    None
}

#[sqlx::test(migrations = "../../migrations")]
async fn unknown_client_route_serves_index_html(pool: PgPool) {
    build_dir();
    let res = app(pool).oneshot(get("/repositories", None)).await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/html");
    assert_eq!(res.headers()[header::CACHE_CONTROL], "no-cache");
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8(body.to_vec()).unwrap().to_lowercase();
    assert!(html.starts_with("<!doctype html"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn unknown_api_route_is_json_404(pool: PgPool) {
    let app = app(pool);
    for uri in ["/api/v1/nope", "/webhooks/nope"] {
        let reply = send(&app, get(uri, None)).await;
        assert_eq!(reply.status, 404, "{uri}");
        assert_eq!(reply.body["error"]["code"], "not_found", "{uri}");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn asset_has_correct_content_type(pool: PgPool) {
    let dir = build_dir();
    let js = first_js(&dir.join("_app/immutable"), &dir).expect("no .js asset in web/build");
    let res = app(pool)
        .oneshot(get(&format!("/{js}"), None))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let content_type = res.headers()[header::CONTENT_TYPE].to_str().unwrap();
    assert!(content_type.contains("javascript"), "{content_type}");
    assert_eq!(
        res.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
}
