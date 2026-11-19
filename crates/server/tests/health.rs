use adus_core::config::Config;
use adus_server::{AppState, router};
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use std::collections::HashMap;
use tower::ServiceExt;

fn test_config() -> Config {
    let env: HashMap<String, String> = [
        ("DATABASE_URL", "postgres://x"),
        ("REDIS_URL", "redis://x"),
        ("ADUS_SECRET_KEY", "k"),
        ("ADUS_PUBLIC_URL", "http://x"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    Config::from_map(&env).unwrap()
}

#[tokio::test]
async fn healthz_returns_ok() {
    let app = router(AppState {
        config: test_config(),
    });
    let res = app
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ok");
}
