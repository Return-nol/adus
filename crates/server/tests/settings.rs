mod common;

use axum::{Router, http::StatusCode};
use common::{app_and_git, cookie_pair, get, json_req, post_json, send};
use serde_json::{Value, json};
use sqlx::PgPool;

const PASSWORD: &str = "correct horse";

async fn setup_admin(app: &Router) -> String {
    let body = json!({ "email": "admin@example.com", "password": PASSWORD });
    let reply = send(app, post_json("/api/v1/auth/setup", &body, None)).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    cookie_pair(&reply)
}

/// PUT /settings with the required fields plus `extra`.
async fn put_settings(app: &Router, cookie: &str, extra: Value) -> common::Reply {
    let mut body = json!({
        "default_provider_id": null,
        "max_files_per_review": 30, "max_patch_lines_per_file": 1500
    });
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    send(
        app,
        json_req("PUT", "/api/v1/settings", &body, Some(cookie)),
    )
    .await
}

async fn read_settings(app: &Router, cookie: &str) -> Value {
    send(app, get("/api/v1/settings", Some(cookie))).await.body
}

#[sqlx::test(migrations = "../../migrations")]
async fn settings_defaults_are_30_files_1500_lines_100_small_pr_2m_tokens(pool: PgPool) {
    let (app, _git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;
    let now = read_settings(&app, &admin).await;
    assert_eq!(now["max_files_per_review"], 30);
    assert_eq!(now["max_patch_lines_per_file"], 1500);
    assert_eq!(now["small_pr_max_lines"], 100);
    assert_eq!(now["max_review_input_tokens"], 2_000_000);
}

#[sqlx::test(migrations = "../../migrations")]
async fn update_settings_validates_max_review_input_tokens(pool: PgPool) {
    let (app, _git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;

    // 0 (no limit) and 100,000,000 are the inclusive bounds.
    for ok in [0, 750_000, 100_000_000] {
        let reply = put_settings(&app, &admin, json!({ "max_review_input_tokens": ok })).await;
        assert_eq!(reply.status, StatusCode::OK, "{ok}");
        assert_eq!(reply.body["max_review_input_tokens"], ok);
    }

    // Out of range: 422 and nothing changes.
    for bad in [-1, 100_000_001] {
        let reply = put_settings(&app, &admin, json!({ "max_review_input_tokens": bad })).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert_eq!(reply.body["error"]["code"], "validation_error");
        assert_eq!(
            reply.body["error"]["message"],
            "max_review_input_tokens must be between 0 and 100000000"
        );
    }
    assert_eq!(
        read_settings(&app, &admin).await["max_review_input_tokens"],
        100_000_000
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn update_without_max_review_input_tokens_keeps_current(pool: PgPool) {
    let (app, _git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;
    let reply = put_settings(&app, &admin, json!({ "max_review_input_tokens": 123_456 })).await;
    assert_eq!(reply.status, StatusCode::OK);

    let reply = put_settings(&app, &admin, json!({ "max_files_per_review": 12 })).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["max_files_per_review"], 12);
    assert_eq!(reply.body["max_review_input_tokens"], 123_456);
}
