mod common;

use adus_git::{RemoteRepo, RepoRef};
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

async fn login_user(app: &Router, admin: &str) -> String {
    let body = json!({ "email": "u@example.com", "password": PASSWORD, "role": "user" });
    let created = send(app, post_json("/api/v1/users", &body, Some(admin))).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let body = json!({ "email": "u@example.com", "password": PASSWORD });
    cookie_pair(&send(app, post_json("/api/v1/auth/login", &body, None)).await)
}

/// PUT /settings with the required fields plus `extra`.
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

/// App, admin cookie and one repository added through the API. Returns its PATCH uri.
async fn setup_with_repo(pool: PgPool) -> (Router, String, String) {
    let (app, git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;
    git.with(|s| {
        s.repos = vec![RemoteRepo {
            r#ref: RepoRef {
                external_id: "1".into(),
                owner: "acme".into(),
                project: None,
                name: "api".into(),
            },
            default_branch: Some("main".into()),
            project_name: None,
        }]
    });
    let conn = json!({
        "kind": "github", "name": "Acme", "org_or_owner": "acme", "token": "ghp_token1111"
    });
    let conn = send(&app, post_json("/api/v1/connections", &conn, Some(&admin))).await;
    assert_eq!(conn.status, StatusCode::CREATED);
    let repo = json!({ "connection_id": conn.body["id"], "external_id": "1" });
    let repo = send(&app, post_json("/api/v1/repositories", &repo, Some(&admin))).await;
    assert_eq!(repo.status, StatusCode::CREATED);
    let uri = format!("/api/v1/repositories/{}", repo.body["id"].as_str().unwrap());
    (app, admin, uri)
}

async fn patch_repo(app: &Router, cookie: &str, uri: &str, body: Value) -> common::Reply {
    send(app, json_req("PATCH", uri, &body, Some(cookie))).await
}

async fn repo_json(app: &Router, cookie: &str) -> Value {
    let list = send(app, get("/api/v1/repositories", Some(cookie))).await;
    list.body[0].clone()
}

#[sqlx::test(migrations = "../../migrations")]
async fn get_settings_defaults_to_generalist_and_put_updates_it(pool: PgPool) {
    let (app, _git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;
    assert_eq!(
        read_settings(&app, &admin).await["review_mode"],
        "generalist"
    );

    let reply = put_settings(&app, &admin, json!({ "review_mode": "specialists" })).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["review_mode"], "specialists");
    assert_eq!(
        read_settings(&app, &admin).await["review_mode"],
        "specialists"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn put_settings_omitting_review_mode_keeps_existing_value(pool: PgPool) {
    let (app, _git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;
    let reply = put_settings(&app, &admin, json!({ "review_mode": "specialists" })).await;
    assert_eq!(reply.status, StatusCode::OK);

    let reply = put_settings(&app, &admin, json!({ "max_files_per_review": 60 })).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["review_mode"], "specialists");
    let now = read_settings(&app, &admin).await;
    assert_eq!(now["review_mode"], "specialists");
    assert_eq!(now["max_files_per_review"], 60);
}

#[sqlx::test(migrations = "../../migrations")]
async fn settings_small_pr_max_lines_defaults_to_50_and_validates_range(pool: PgPool) {
    let (app, _git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;
    assert_eq!(read_settings(&app, &admin).await["small_pr_max_lines"], 50);

    let reply = put_settings(&app, &admin, json!({ "small_pr_max_lines": 120 })).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["small_pr_max_lines"], 120);

    // Omitting the field keeps the value.
    let reply = put_settings(&app, &admin, json!({ "max_files_per_review": 60 })).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["small_pr_max_lines"], 120);

    // 0 (never small) and 10000 are the inclusive bounds.
    for ok in [0, 10_000] {
        let reply = put_settings(&app, &admin, json!({ "small_pr_max_lines": ok })).await;
        assert_eq!(reply.status, StatusCode::OK, "{ok}");
        assert_eq!(reply.body["small_pr_max_lines"], ok);
    }

    // Out of range: 422 and nothing changes.
    for bad in [-1, 10_001] {
        let reply = put_settings(&app, &admin, json!({ "small_pr_max_lines": bad })).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert_eq!(reply.body["error"]["code"], "validation_error");
    }
    assert_eq!(
        read_settings(&app, &admin).await["small_pr_max_lines"],
        10_000
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn patch_repo_sets_and_clears_review_mode_override(pool: PgPool) {
    let (app, admin, uri) = setup_with_repo(pool).await;
    assert!(repo_json(&app, &admin).await["review_mode"].is_null());

    let reply = patch_repo(&app, &admin, &uri, json!({ "review_mode": "specialists" })).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["review_mode"], "specialists");
    assert_eq!(repo_json(&app, &admin).await["review_mode"], "specialists");

    // Omitting the field leaves the override alone.
    let reply = patch_repo(&app, &admin, &uri, json!({ "auto_review": false })).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["review_mode"], "specialists");
    assert_eq!(reply.body["auto_review"], false);

    // `null` clears it (inherit the global default).
    let reply = patch_repo(&app, &admin, &uri, json!({ "review_mode": null })).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body["review_mode"].is_null());
    assert!(repo_json(&app, &admin).await["review_mode"].is_null());
}

#[sqlx::test(migrations = "../../migrations")]
async fn invalid_review_mode_returns_422(pool: PgPool) {
    let (app, admin, uri) = setup_with_repo(pool).await;

    let reply = put_settings(&app, &admin, json!({ "review_mode": "turbo" })).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "validation_error");
    assert_eq!(
        read_settings(&app, &admin).await["review_mode"],
        "generalist"
    );

    let reply = patch_repo(&app, &admin, &uri, json!({ "review_mode": "turbo" })).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "validation_error");
    assert!(repo_json(&app, &admin).await["review_mode"].is_null());
}

#[sqlx::test(migrations = "../../migrations")]
async fn user_role_cannot_change_review_mode_403(pool: PgPool) {
    let (app, admin, uri) = setup_with_repo(pool).await;
    let user = login_user(&app, &admin).await;

    let reply = put_settings(&app, &user, json!({ "review_mode": "specialists" })).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = patch_repo(&app, &user, &uri, json!({ "review_mode": "specialists" })).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);

    // Reading is allowed and nothing changed.
    assert_eq!(
        read_settings(&app, &user).await["review_mode"],
        "generalist"
    );
    assert!(repo_json(&app, &user).await["review_mode"].is_null());
}
