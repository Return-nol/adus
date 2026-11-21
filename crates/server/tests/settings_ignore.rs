mod common;

use axum::{Router, http::StatusCode};
use common::{app, cookie_pair, get, json_req, post_json, send};
use serde_json::{Value, json};
use sqlx::PgPool;

const PASSWORD: &str = "correct horse";

async fn setup_admin(app: &Router) -> String {
    let body = json!({ "email": "admin@example.com", "password": PASSWORD });
    let reply = send(app, post_json("/api/v1/auth/setup", &body, None)).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    cookie_pair(&reply)
}

async fn login_user(app: &Router, admin: &str) -> String {
    let body = json!({ "email": "u@example.com", "password": PASSWORD, "role": "user" });
    let created = send(app, post_json("/api/v1/users", &body, Some(admin))).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let body = json!({ "email": "u@example.com", "password": PASSWORD });
    cookie_pair(&send(app, post_json("/api/v1/auth/login", &body, None)).await)
}

/// PUT /settings dengan field wajib bawaan ditambah `extra`.
async fn put_settings(app: &Router, cookie: &str, extra: Value) -> common::Reply {
    let mut body = json!({
        "default_provider_id": null,
        "max_files_per_review": 50, "max_patch_lines_per_file": 2000
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

async fn ignore_test(app: &Router, cookie: &str, path: &str) -> common::Reply {
    let body = json!({ "path": path });
    send(
        app,
        post_json("/api/v1/settings/ignore-test", &body, Some(cookie)),
    )
    .await
}

#[sqlx::test(migrations = "../../migrations")]
async fn get_settings_returns_defaults(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let body = read_settings(&app, &admin).await;
    assert_eq!(body["ignore_paths_is_default"], true);
    assert!(body["ignore_paths"].as_array().unwrap().len() > 1000);
    assert_eq!(body["ignored_title_keywords"], json!([]));
}

#[sqlx::test(migrations = "../../migrations")]
async fn put_custom_list_then_reset(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;

    let saved = put_settings(
        &app,
        &admin,
        json!({
            "ignore_paths": ["  dist/** ", "", "dist/**", "docs/**"],
            "ignored_title_keywords": [" WIP ", "wip", "", "[skip]"]
        }),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK);
    assert_eq!(saved.body["ignore_paths"], json!(["dist/**", "docs/**"]));
    assert_eq!(saved.body["ignore_paths_is_default"], false);
    assert_eq!(
        saved.body["ignored_title_keywords"],
        json!(["WIP", "[skip]"])
    );
    assert_eq!(read_settings(&app, &admin).await, saved.body);

    // Field tidak dikirim = tidak berubah.
    let kept = put_settings(&app, &admin, json!({})).await;
    assert_eq!(kept.body, saved.body);

    // Array kosong = daftar kosong yang sah (bukan bawaan).
    let empty = put_settings(&app, &admin, json!({ "ignore_paths": [] })).await;
    assert_eq!(empty.body["ignore_paths"], json!([]));
    assert_eq!(empty.body["ignore_paths_is_default"], false);

    // null = kembali ke bawaan; kata kunci tetap.
    let reset = put_settings(&app, &admin, json!({ "ignore_paths": null })).await;
    assert_eq!(reset.status, StatusCode::OK);
    assert_eq!(reset.body["ignore_paths_is_default"], true);
    assert!(reset.body["ignore_paths"].as_array().unwrap().len() > 1000);
    assert_eq!(
        reset.body["ignored_title_keywords"],
        json!(["WIP", "[skip]"])
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn put_with_one_bad_glob_saves_nothing(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let before = read_settings(&app, &admin).await;

    let mut list = before["ignore_paths"].as_array().unwrap().clone();
    list.push(json!("a[b"));
    let reply = put_settings(
        &app,
        &admin,
        json!({ "ignore_paths": list, "ignored_title_keywords": ["wip"] }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "invalid_glob");
    assert_eq!(reply.body["error"]["message"], "invalid glob pattern: a[b");
    assert_eq!(read_settings(&app, &admin).await, before);
}

#[sqlx::test(migrations = "../../migrations")]
async fn put_list_too_complex_to_compile_saves_nothing(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let before = read_settings(&app, &admin).await;

    // Every pattern is a valid glob, but together they exceed the GlobSet size limit.
    let list: Vec<String> = (0..300)
        .map(|i| format!("{}{i}", "?".repeat(490)))
        .collect();
    let reply = put_settings(&app, &admin, json!({ "ignore_paths": list })).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "invalid_glob");
    assert!(
        reply.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("too large or complex"),
        "{}",
        reply.body
    );
    assert_eq!(read_settings(&app, &admin).await, before);
}

#[sqlx::test(migrations = "../../migrations")]
async fn put_strips_leading_slash(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let saved = put_settings(
        &app,
        &admin,
        json!({ "ignore_paths": ["/dist/**", "dist/**"] }),
    )
    .await;
    assert_eq!(saved.body["ignore_paths"], json!(["dist/**"]));
}

#[sqlx::test(migrations = "../../migrations")]
async fn title_keyword_too_long_422(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let before = read_settings(&app, &admin).await;

    let long = "x".repeat(101);
    let reply = put_settings(
        &app,
        &admin,
        json!({ "ignore_paths": ["dist/**"], "ignored_title_keywords": [long] }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "validation_error");
    assert!(
        reply.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&long)
    );
    assert_eq!(read_settings(&app, &admin).await, before);
}

#[sqlx::test(migrations = "../../migrations")]
async fn ignore_test_reports_pattern(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let user = login_user(&app, &admin).await;

    // Daftar bawaan, dipanggil oleh user biasa.
    let lock = ignore_test(&app, &user, "/yarn.lock").await;
    assert_eq!(lock.status, StatusCode::OK);
    assert_eq!(lock.body["ignored"], true);
    assert!(lock.body["pattern"].is_string());
    let src = ignore_test(&app, &user, "src/main.rs").await;
    assert_eq!(src.body, json!({ "ignored": false, "pattern": null }));

    // Daftar kustom.
    put_settings(&app, &admin, json!({ "ignore_paths": ["dist/**"] })).await;
    let hit = ignore_test(&app, &user, "dist/a.js").await;
    assert_eq!(hit.body, json!({ "ignored": true, "pattern": "dist/**" }));
    let miss = ignore_test(&app, &user, "yarn.lock").await;
    assert_eq!(miss.body["ignored"], false);

    for path in ["", "   ", &"p".repeat(1001)] {
        let reply = ignore_test(&app, &user, path).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    }
    let anon = send(
        &app,
        post_json(
            "/api/v1/settings/ignore-test",
            &json!({ "path": "a" }),
            None,
        ),
    )
    .await;
    assert_eq!(anon.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations")]
async fn user_role_cannot_put_ignore_403(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let user = login_user(&app, &admin).await;
    let reply = put_settings(&app, &user, json!({ "ignore_paths": ["dist/**"] })).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(
        read_settings(&app, &admin).await["ignore_paths_is_default"],
        true
    );
}
