mod common;

use common::{app, get};
use http_body_util::BodyExt;
use sqlx::PgPool;
use tower::ServiceExt;

#[sqlx::test(migrations = "../../migrations")]
async fn healthz_returns_ok(pool: PgPool) {
    let res = app(pool).oneshot(get("/healthz", None)).await.unwrap();
    assert_eq!(res.status(), 200);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ok");
}
