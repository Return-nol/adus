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

async fn ignore_test(app: &Router, cookie: &str, path: &str) -> common::Reply {
    let body = json!({ "path": path });
    send(
        app,
        post_json("/api/v1/settings/ignore-test", &body, Some(cookie)),
    )
    .await
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
async fn settings_defaults_include_limits_review_mode_and_ignore_paths(pool: PgPool) {
    let (app, _git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;
    let now = read_settings(&app, &admin).await;
    assert_eq!(now["max_files_per_review"], 30);
    assert_eq!(now["max_patch_lines_per_file"], 1500);
    assert_eq!(now["small_pr_max_lines"], 100);
    assert_eq!(now["max_review_input_tokens"], 2_000_000);
    assert_eq!(now["review_mode"], "generalist");
    assert_eq!(now["ignore_paths_is_default"], true);
    assert!(now["ignore_paths"].as_array().unwrap().len() > 1000);
    assert_eq!(now["ignored_title_keywords"], json!([]));
}

#[sqlx::test(migrations = "../../migrations")]
async fn update_settings_persists_fields_and_omitting_optional_fields_keeps_existing(pool: PgPool) {
    let (app, _git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;

    let reply = put_settings(
        &app,
        &admin,
        json!({
            "review_mode": "specialists",
            "small_pr_max_lines": 120,
            "max_review_input_tokens": 123_456,
            "ignore_paths": ["dist/**"],
            "ignored_title_keywords": ["WIP"]
        }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["review_mode"], "specialists");
    assert_eq!(reply.body["small_pr_max_lines"], 120);
    assert_eq!(reply.body["max_review_input_tokens"], 123_456);

    // Omitting optional fields keeps every existing value intact.
    let kept = put_settings(&app, &admin, json!({ "max_files_per_review": 60 })).await;
    assert_eq!(kept.status, StatusCode::OK);
    assert_eq!(kept.body["max_files_per_review"], 60);
    assert_eq!(kept.body["review_mode"], "specialists");
    assert_eq!(kept.body["small_pr_max_lines"], 120);
    assert_eq!(kept.body["max_review_input_tokens"], 123_456);
    assert_eq!(kept.body["ignore_paths"], json!(["dist/**"]));
    assert_eq!(kept.body["ignored_title_keywords"], json!(["WIP"]));
    assert_eq!(read_settings(&app, &admin).await, kept.body);
}

#[sqlx::test(migrations = "../../migrations")]
async fn update_settings_validates_small_pr_max_lines_and_max_review_input_tokens(pool: PgPool) {
    let (app, _git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;

    // 0 (no limit) and 100,000,000 are the inclusive bounds for max_review_input_tokens.
    for ok in [0, 750_000, 100_000_000] {
        let reply = put_settings(&app, &admin, json!({ "max_review_input_tokens": ok })).await;
        assert_eq!(reply.status, StatusCode::OK, "{ok}");
        assert_eq!(reply.body["max_review_input_tokens"], ok);
    }
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

    // 0 (never small) and 10000 are the inclusive bounds for small_pr_max_lines.
    for ok in [0, 10_000] {
        let reply = put_settings(&app, &admin, json!({ "small_pr_max_lines": ok })).await;
        assert_eq!(reply.status, StatusCode::OK, "{ok}");
        assert_eq!(reply.body["small_pr_max_lines"], ok);
    }
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
async fn user_role_cannot_modify_settings_or_repo_review_mode_403(pool: PgPool) {
    let (app, admin, uri) = setup_with_repo(pool).await;
    let user = login_user(&app, &admin).await;

    for extra in [
        json!({ "review_mode": "specialists" }),
        json!({ "ignore_paths": ["dist/**"] }),
    ] {
        let reply = put_settings(&app, &user, extra).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
    }
    let reply = patch_repo(&app, &user, &uri, json!({ "review_mode": "specialists" })).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);

    // Reading is allowed and nothing changed.
    let current = read_settings(&app, &user).await;
    assert_eq!(current["review_mode"], "generalist");
    assert_eq!(current["ignore_paths_is_default"], true);
    assert!(repo_json(&app, &user).await["review_mode"].is_null());
}

#[sqlx::test(migrations = "../../migrations")]
async fn put_custom_ignore_list_strips_slashes_and_resets(pool: PgPool) {
    let (app, _git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;

    let saved = put_settings(
        &app,
        &admin,
        json!({
            "ignore_paths": ["  dist/** ", "/dist/**", "", "dist/**", "docs/**"],
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

    // Empty array = valid empty list (not defaults).
    let empty = put_settings(&app, &admin, json!({ "ignore_paths": [] })).await;
    assert_eq!(empty.body["ignore_paths"], json!([]));
    assert_eq!(empty.body["ignore_paths_is_default"], false);

    // null = back to defaults; title keywords remain.
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
async fn put_invalid_ignore_glob_or_title_keyword_saves_nothing(pool: PgPool) {
    let (app, _git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;
    let before = read_settings(&app, &admin).await;

    // One bad glob in the list rejects the whole update.
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

    // Every pattern valid individually, but together they exceed the GlobSet size limit.
    let complex: Vec<String> = (0..300)
        .map(|i| format!("{}{i}", "?".repeat(490)))
        .collect();
    let reply = put_settings(&app, &admin, json!({ "ignore_paths": complex })).await;
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

    // Title keyword over 100 chars is rejected.
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
    let (app, _git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;
    let user = login_user(&app, &admin).await;

    // Defaults, called by a regular user.
    let lock = ignore_test(&app, &user, "/yarn.lock").await;
    assert_eq!(lock.status, StatusCode::OK);
    assert_eq!(lock.body["ignored"], true);
    assert!(lock.body["pattern"].is_string());
    let src = ignore_test(&app, &user, "src/main.rs").await;
    assert_eq!(src.body, json!({ "ignored": false, "pattern": null }));

    // Custom list.
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
