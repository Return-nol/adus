mod common;

use adus_core::types::ConnectionKind;
use adus_db::{
    connections,
    memories::{self, MemoryKind, MemorySource, MemoryStatus, NewMemory},
    repositories::{self, NewRepository},
    rules::KnowledgeScope,
};
use axum::{Router, http::StatusCode};
use common::{app, cookie_pair, get, json_req, post_empty, post_json, send};
use serde_json::{Value, json};
use sqlx::PgPool;

const PASSWORD: &str = "correct horse";

async fn setup_admin(app: &Router) -> String {
    let body = json!({ "email": "admin@example.com", "password": PASSWORD });
    let reply = send(app, post_json("/api/v1/auth/setup", &body, None)).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    cookie_pair(&reply)
}

/// Admin membuat user biasa lalu login; mengembalikan cookie user itu.
async fn login_as_user(app: &Router, admin: &str) -> String {
    let body = json!({ "email": "u@example.com", "password": PASSWORD, "role": "user" });
    let created = send(app, post_json("/api/v1/users", &body, Some(admin))).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let login = json!({ "email": "u@example.com", "password": PASSWORD });
    let reply = send(app, post_json("/api/v1/auth/login", &login, None)).await;
    cookie_pair(&reply)
}

/// Repo aktif langsung lewat DB (tanpa provider git); mengembalikan id-nya.
async fn add_repo(pool: &PgPool, external_id: &str) -> String {
    let conn = connections::insert(
        pool,
        ConnectionKind::Github,
        "c",
        "acme",
        "enc",
        "1234",
        "octocat",
    )
    .await
    .unwrap();
    let repo = repositories::insert(
        pool,
        NewRepository {
            connection_id: conn.id,
            external_id,
            owner: "acme",
            project: None,
            project_name: None,
            name: external_id,
            default_branch: None,
            webhook_token: "t",
        },
    )
    .await
    .unwrap()
    .unwrap();
    repo.id.to_string()
}

async fn post(app: &Router, uri: &str, body: &Value, cookie: &str) -> common::Reply {
    send(app, post_json(uri, body, Some(cookie))).await
}

async fn patch(app: &Router, uri: &str, body: &Value, cookie: &str) -> common::Reply {
    send(app, json_req("PATCH", uri, body, Some(cookie))).await
}

async fn delete(app: &Router, uri: &str, cookie: &str) -> common::Reply {
    send(app, json_req("DELETE", uri, &Value::Null, Some(cookie))).await
}

fn ids(body: &Value) -> Vec<&str> {
    body.as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect()
}

#[sqlx::test(migrations = "../../migrations")]
async fn rules_require_login(pool: PgPool) {
    let app = app(pool).await;
    assert_eq!(
        send(&app, get("/api/v1/rules", None)).await.status,
        StatusCode::UNAUTHORIZED
    );
    let body = json!({ "scope": "global", "title": "t", "body": "b" });
    assert_eq!(
        send(&app, post_json("/api/v1/rules", &body, None))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn user_role_can_create_rule(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let user = login_as_user(&app, &admin).await;

    let body = json!({
        "scope": "global", "title": " No unwrap ", "body": "Do not unwrap in handlers.",
        "path_glob": " src/** ", "severity": "critical"
    });
    let reply = post(&app, "/api/v1/rules", &body, &user).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.body["scope"], "global");
    assert_eq!(reply.body["repository_id"], Value::Null);
    assert_eq!(reply.body["title"], "No unwrap");
    assert_eq!(reply.body["path_glob"], "src/**");
    assert_eq!(reply.body["severity"], "critical");
    assert_eq!(reply.body["enabled"], true);
    assert!(reply.body["created_at"].is_string() && reply.body["updated_at"].is_string());

    // Severity dan glob opsional: default warning, tanpa glob.
    let body = json!({ "scope": "global", "title": "t", "body": "b" });
    let reply = post(&app, "/api/v1/rules", &body, &user).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.body["severity"], "warning");
    assert_eq!(reply.body["path_glob"], Value::Null);
}

#[sqlx::test(migrations = "../../migrations")]
async fn invalid_glob_422(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;

    let body = json!({ "scope": "global", "title": "t", "body": "b", "path_glob": "src/[a" });
    let reply = post(&app, "/api/v1/rules", &body, &admin).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "invalid_glob");

    let ok = json!({ "scope": "global", "title": "t", "body": "b" });
    let created = post(&app, "/api/v1/rules", &ok, &admin).await;
    let uri = format!("/api/v1/rules/{}", created.body["id"].as_str().unwrap());
    let reply = patch(&app, &uri, &json!({ "path_glob": "src/[a" }), &admin).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "invalid_glob");
}

#[sqlx::test(migrations = "../../migrations")]
async fn repo_scope_without_repository_id_422(pool: PgPool) {
    let repo = add_repo(&pool, "a").await;
    let app = app(pool).await;
    let admin = setup_admin(&app).await;

    let body = json!({ "scope": "repo", "title": "t", "body": "b" });
    let reply = post(&app, "/api/v1/rules", &body, &admin).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "validation_error");

    // Global dengan repository_id juga salah.
    let body = json!({ "scope": "global", "repository_id": repo, "title": "t", "body": "b" });
    let reply = post(&app, "/api/v1/rules", &body, &admin).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "validation_error");
}

#[sqlx::test(migrations = "../../migrations")]
async fn unknown_or_deleted_repository_404(pool: PgPool) {
    let repo = add_repo(&pool, "a").await;
    let app_router = app(pool.clone()).await;
    let admin = setup_admin(&app_router).await;

    let body = json!({
        "scope": "repo", "repository_id": uuid_nil(), "title": "t", "body": "b"
    });
    let reply = post(&app_router, "/api/v1/rules", &body, &admin).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body["error"]["code"], "not_found");

    repositories::soft_delete(&pool, repo.parse().unwrap())
        .await
        .unwrap();
    let body = json!({ "scope": "repo", "repository_id": repo, "title": "t", "body": "b" });
    let reply = post(&app_router, "/api/v1/rules", &body, &admin).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    let reply = send(
        &app_router,
        get(&format!("/api/v1/rules?repository_id={repo}"), Some(&admin)),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

fn uuid_nil() -> &'static str {
    "00000000-0000-0000-0000-000000000000"
}

#[sqlx::test(migrations = "../../migrations")]
async fn rule_text_validation_422(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;

    let cases = [
        json!({ "scope": "global", "title": "  ", "body": "b" }),
        json!({ "scope": "global", "title": "t", "body": "  " }),
        json!({ "scope": "global", "title": "x".repeat(201), "body": "b" }),
        json!({ "scope": "global", "title": "t", "body": "x".repeat(10_001) }),
    ];
    for body in cases {
        let reply = post(&app, "/api/v1/rules", &body, &admin).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(reply.body["error"]["code"], "validation_error");
    }
    // Batas atas masih boleh.
    let body = json!({ "scope": "global", "title": "x".repeat(200), "body": "x".repeat(10_000) });
    assert_eq!(
        post(&app, "/api/v1/rules", &body, &admin).await.status,
        StatusCode::CREATED
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn list_rules_by_scope(pool: PgPool) {
    let repo = add_repo(&pool, "a").await;
    let app = app(pool).await;
    let admin = setup_admin(&app).await;

    let g = json!({ "scope": "global", "title": "g", "body": "b" });
    let r = json!({ "scope": "repo", "repository_id": repo, "title": "r", "body": "b" });
    let global_id = post(&app, "/api/v1/rules", &g, &admin).await.body["id"].clone();
    let repo_rule = post(&app, "/api/v1/rules", &r, &admin).await;
    assert_eq!(repo_rule.status, StatusCode::CREATED);
    assert_eq!(repo_rule.body["repository_id"], repo);
    let repo_id = repo_rule.body["id"].clone();

    let reply = send(&app, get("/api/v1/rules", Some(&admin))).await;
    assert_eq!(ids(&reply.body), [global_id.as_str().unwrap()]);

    let uri = format!("/api/v1/rules?repository_id={repo}");
    let reply = send(&app, get(&uri, Some(&admin))).await;
    assert_eq!(ids(&reply.body), [repo_id.as_str().unwrap()]);

    let reply = send(&app, get("/api/v1/rules?all=true", Some(&admin))).await;
    assert_eq!(reply.body.as_array().unwrap().len(), 2);
}

#[sqlx::test(migrations = "../../migrations")]
async fn patch_rule_is_partial_and_can_clear_glob(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let body = json!({
        "scope": "global", "title": "t", "body": "b", "path_glob": "src/**", "severity": "info"
    });
    let created = post(&app, "/api/v1/rules", &body, &admin).await;
    let uri = format!("/api/v1/rules/{}", created.body["id"].as_str().unwrap());

    let reply = patch(
        &app,
        &uri,
        &json!({ "enabled": false, "title": " new " }),
        &admin,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["enabled"], false);
    assert_eq!(reply.body["title"], "new");
    assert_eq!(reply.body["body"], "b");
    assert_eq!(reply.body["path_glob"], "src/**");
    assert_eq!(reply.body["severity"], "info");
    // Scope tidak bisa diubah lewat PATCH.
    let reply = patch(&app, &uri, &json!({ "scope": "repo" }), &admin).await;
    assert_eq!(reply.body["scope"], "global");

    let reply = patch(&app, &uri, &json!({ "path_glob": "" }), &admin).await;
    assert_eq!(reply.body["path_glob"], Value::Null);
    let reply = patch(&app, &uri, &json!({ "path_glob": "*.rs" }), &admin).await;
    assert_eq!(reply.body["path_glob"], "*.rs");
    let reply = patch(&app, &uri, &json!({ "path_glob": null }), &admin).await;
    assert_eq!(reply.body["path_glob"], Value::Null);

    let reply = patch(&app, &uri, &json!({ "title": "" }), &admin).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);

    let missing = format!("/api/v1/rules/{}", uuid_nil());
    let reply = patch(&app, &missing, &json!({ "enabled": true }), &admin).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../../migrations")]
async fn user_role_can_delete_rule(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let user = login_as_user(&app, &admin).await;
    let body = json!({ "scope": "global", "title": "t", "body": "b" });
    let created = post(&app, "/api/v1/rules", &body, &admin).await;
    let uri = format!("/api/v1/rules/{}", created.body["id"].as_str().unwrap());

    assert_eq!(
        delete(&app, &uri, &user).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        delete(&app, &uri, &user).await.status,
        StatusCode::NOT_FOUND
    );
    let reply = send(&app, get("/api/v1/rules", Some(&user))).await;
    assert!(reply.body.as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn memory_create_sets_source_and_creator(pool: PgPool) {
    let repo = add_repo(&pool, "a").await;
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let user = login_as_user(&app, &admin).await;
    let me = send(&app, get("/api/v1/auth/me", Some(&user))).await;

    // Client tidak bisa mengisi source / created_by / source_url.
    let body = json!({
        "scope": "repo", "repository_id": repo, "content": " Use anyhow for errors. ",
        "source": "pr_comment", "source_url": "http://x", "created_by": uuid_nil()
    });
    let reply = post(&app, "/api/v1/memories", &body, &user).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.body["content"], "Use anyhow for errors.");
    assert_eq!(reply.body["source"], "manual");
    assert_eq!(reply.body["source_url"], Value::Null);
    assert_eq!(reply.body["created_by"]["id"], me.body["id"]);
    assert_eq!(reply.body["created_by"]["email"], "u@example.com");
    assert_eq!(reply.body["repository_id"], repo);
    assert_eq!(reply.body["enabled"], true);
    assert!(reply.body["created_at"].is_string());
}

#[sqlx::test(migrations = "../../migrations")]
async fn memory_validation_422_and_scope_mismatch(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;

    for body in [
        json!({ "scope": "global", "content": "  " }),
        json!({ "scope": "global", "content": "x".repeat(2001) }),
        json!({ "scope": "repo", "content": "c" }),
        json!({ "scope": "global", "repository_id": uuid_nil(), "content": "c" }),
    ] {
        let reply = post(&app, "/api/v1/memories", &body, &admin).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(reply.body["error"]["code"], "validation_error");
    }
    let body = json!({ "scope": "repo", "repository_id": uuid_nil(), "content": "c" });
    let reply = post(&app, "/api/v1/memories", &body, &admin).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    let body = json!({ "scope": "global", "content": "x".repeat(2000) });
    assert_eq!(
        post(&app, "/api/v1/memories", &body, &admin).await.status,
        StatusCode::CREATED
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn memory_list_patch_delete(pool: PgPool) {
    let repo = add_repo(&pool, "a").await;
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let user = login_as_user(&app, &admin).await;

    let g = json!({ "scope": "global", "content": "global one" });
    let r = json!({ "scope": "repo", "repository_id": repo, "content": "repo one" });
    let global = post(&app, "/api/v1/memories", &g, &user).await.body["id"].clone();
    let scoped = post(&app, "/api/v1/memories", &r, &user).await.body["id"].clone();

    let reply = send(&app, get("/api/v1/memories", Some(&user))).await;
    assert_eq!(ids(&reply.body), [global.as_str().unwrap()]);
    let uri = format!("/api/v1/memories?repository_id={repo}");
    let reply = send(&app, get(&uri, Some(&user))).await;
    assert_eq!(ids(&reply.body), [scoped.as_str().unwrap()]);
    let reply = send(&app, get("/api/v1/memories?all=true", Some(&user))).await;
    assert_eq!(reply.body.as_array().unwrap().len(), 2);

    let uri = format!("/api/v1/memories/{}", global.as_str().unwrap());
    let reply = patch(&app, &uri, &json!({ "enabled": false }), &user).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["enabled"], false);
    assert_eq!(reply.body["content"], "global one");
    let reply = patch(&app, &uri, &json!({ "content": " edited " }), &user).await;
    assert_eq!(reply.body["content"], "edited");
    let reply = patch(&app, &uri, &json!({ "content": "" }), &user).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);

    assert_eq!(
        delete(&app, &uri, &user).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        delete(&app, &uri, &user).await.status,
        StatusCode::NOT_FOUND
    );
    let reply = patch(&app, &uri, &json!({ "enabled": true }), &user).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

/// Memory hasil belajar langsung lewat DB; mengembalikan id-nya.
async fn learned_memory(pool: &PgPool, content: &str, status: MemoryStatus) -> String {
    let memory = memories::insert(
        pool,
        NewMemory {
            scope: KnowledgeScope::Global,
            repository_id: None,
            content,
            source: MemorySource::PrLearning,
            source_url: Some("https://example.com/pr/1"),
            created_by: None,
            status,
            kind: MemoryKind::Convention,
            context: None,
            reason: None,
            resolution: None,
            evidence: &[],
            supersedes: None,
        },
    )
    .await
    .unwrap();
    memory.id.to_string()
}

#[sqlx::test(migrations = "../../migrations")]
async fn approve_requires_admin_and_pending(pool: PgPool) {
    let id = learned_memory(&pool, "learned", MemoryStatus::Pending).await;
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let user = login_as_user(&app, &admin).await;
    let uri = format!("/api/v1/memories/{id}/approve");

    assert_eq!(
        post(&app, &uri, &json!({}), &user).await.status,
        StatusCode::FORBIDDEN
    );
    let reply = post(&app, &uri, &json!({ "content": "  " }), &admin).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "validation_error");

    let reply = post(&app, &uri, &json!({ "content": " edited " }), &admin).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["status"], "active");
    assert_eq!(reply.body["source"], "pr_learning");
    assert_eq!(reply.body["content"], "edited");

    let reply = post(&app, &uri, &json!({}), &admin).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["error"]["code"], "invalid_status");

    let uri = format!("/api/v1/memories/{}/approve", uuid_nil());
    assert_eq!(
        post(&app, &uri, &json!({}), &admin).await.status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn reject_marks_rejected_and_hides_from_default_list(pool: PgPool) {
    let id = learned_memory(&pool, "learned", MemoryStatus::Pending).await;
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let uri = format!("/api/v1/memories/{id}/reject");

    let reply = send(&app, post_empty(&uri, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["status"], "rejected");
    let reply = send(&app, post_empty(&uri, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["error"]["code"], "invalid_status");

    let reply = send(&app, get("/api/v1/memories", Some(&admin))).await;
    assert!(reply.body.as_array().unwrap().is_empty());
    let reply = send(&app, get("/api/v1/memories?status=rejected", Some(&admin))).await;
    assert_eq!(ids(&reply.body), [id.as_str()]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn user_cannot_patch_or_delete_pending_memory(pool: PgPool) {
    let pending = learned_memory(&pool, "pending", MemoryStatus::Pending).await;
    let rejected = learned_memory(&pool, "rejected", MemoryStatus::Rejected).await;
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let user = login_as_user(&app, &admin).await;

    for id in [&pending, &rejected] {
        let uri = format!("/api/v1/memories/{id}");
        let reply = patch(&app, &uri, &json!({ "enabled": false }), &user).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(
            delete(&app, &uri, &user).await.status,
            StatusCode::FORBIDDEN
        );
    }
    let uri = format!("/api/v1/memories/{pending}");
    let reply = patch(&app, &uri, &json!({ "enabled": false }), &admin).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        delete(&app, &uri, &admin).await.status,
        StatusCode::NO_CONTENT
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn user_can_still_patch_active_memory(pool: PgPool) {
    let id = learned_memory(&pool, "active", MemoryStatus::Active).await;
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let user = login_as_user(&app, &admin).await;
    let uri = format!("/api/v1/memories/{id}");

    let reply = patch(&app, &uri, &json!({ "content": "changed" }), &user).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["status"], "active");
    assert_eq!(
        delete(&app, &uri, &user).await.status,
        StatusCode::NO_CONTENT
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn list_filters_by_status(pool: PgPool) {
    let pending = learned_memory(&pool, "p", MemoryStatus::Pending).await;
    let active = learned_memory(&pool, "a", MemoryStatus::Active).await;
    learned_memory(&pool, "r", MemoryStatus::Rejected).await;
    let app = app(pool).await;
    let admin = setup_admin(&app).await;

    let reply = send(&app, get("/api/v1/memories?status=pending", Some(&admin))).await;
    assert_eq!(ids(&reply.body), [pending.as_str()]);
    let uri = "/api/v1/memories?all=true&status=active";
    let reply = send(&app, get(uri, Some(&admin))).await;
    assert_eq!(ids(&reply.body), [active.as_str()]);
    let reply = send(&app, get("/api/v1/memories", Some(&admin))).await;
    assert_eq!(reply.body.as_array().unwrap().len(), 2);

    let reply = send(&app, get("/api/v1/memories?status=nope", Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "validation_error");
}

#[sqlx::test(migrations = "../../migrations")]
async fn memory_roundtrips_new_fields(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;

    // Create with new optional fields (trimmed) and kind; blank optional text -> null.
    let body = json!({
        "scope": "global",
        "content": "Avoid blocking calls",
        "kind": "fix_pattern",
        "context": "  Found in handler  ",
        "reason": "   ",
        "resolution": "  Use spawn_blocking  "
    });
    let created = post(&app, "/api/v1/memories", &body, &admin).await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.body["kind"], "fix_pattern");
    assert_eq!(created.body["context"], "Found in handler");
    assert_eq!(created.body["reason"], Value::Null);
    assert_eq!(created.body["resolution"], "Use spawn_blocking");
    assert_eq!(created.body["evidence"], json!([]));
    assert_eq!(created.body["supersedes"], Value::Null);
    assert_eq!(created.body["superseded_by"], Value::Null);

    let id = created.body["id"].as_str().unwrap();
    let list = send(&app, get("/api/v1/memories", Some(&admin))).await;
    assert_eq!(list.body[0]["id"], id);
    assert_eq!(list.body[0]["kind"], "fix_pattern");
    assert_eq!(list.body[0]["context"], "Found in handler");

    // Patch: update kind and reason, omit context (kept), clear resolution with null.
    let uri = format!("/api/v1/memories/{id}");
    let patched = patch(
        &app,
        &uri,
        &json!({ "kind": "exception", "reason": "  Perf critical  ", "resolution": null }),
        &admin,
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK);
    assert_eq!(patched.body["kind"], "exception");
    assert_eq!(patched.body["context"], "Found in handler");
    assert_eq!(patched.body["reason"], "Perf critical");
    assert_eq!(patched.body["resolution"], Value::Null);

    // Patch: clear context with blank string.
    let cleared = patch(&app, &uri, &json!({ "context": "  " }), &admin).await;
    assert_eq!(cleared.status, StatusCode::OK);
    assert_eq!(cleared.body["context"], Value::Null);
    assert_eq!(cleared.body["reason"], "Perf critical");

    // Validation: > 2000 chars on optional fields -> 422.
    let too_long = "x".repeat(2001);
    let bad_create = post(
        &app,
        "/api/v1/memories",
        &json!({ "scope": "global", "content": "ok", "context": too_long }),
        &admin,
    )
    .await;
    assert_eq!(bad_create.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bad_create.body["error"]["code"], "validation_error");

    let bad_patch = patch(&app, &uri, &json!({ "resolution": too_long }), &admin).await;
    assert_eq!(bad_patch.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bad_patch.body["error"]["code"], "validation_error");
}

#[sqlx::test(migrations = "../../migrations")]
async fn list_filters_by_kind(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;

    let conv = post(
        &app,
        "/api/v1/memories",
        &json!({ "scope": "global", "content": "c" }),
        &admin,
    )
    .await;
    assert_eq!(conv.body["kind"], "convention");
    let exc = post(
        &app,
        "/api/v1/memories",
        &json!({ "scope": "global", "content": "e", "kind": "exception" }),
        &admin,
    )
    .await;
    let fix = post(
        &app,
        "/api/v1/memories",
        &json!({ "scope": "global", "content": "f", "kind": "fix_pattern" }),
        &admin,
    )
    .await;

    for (kind, expected_id) in [
        ("convention", conv.body["id"].as_str().unwrap()),
        ("exception", exc.body["id"].as_str().unwrap()),
        ("fix_pattern", fix.body["id"].as_str().unwrap()),
    ] {
        let reply = send(
            &app,
            get(&format!("/api/v1/memories?kind={kind}"), Some(&admin)),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(ids(&reply.body), [expected_id]);
    }

    let bad = send(&app, get("/api/v1/memories?kind=nope", Some(&admin))).await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bad.body["error"]["code"], "validation_error");
}

#[sqlx::test(migrations = "../../migrations")]
async fn approve_supersedes_target(pool: PgPool) {
    let target = memories::insert(
        &pool,
        NewMemory {
            scope: KnowledgeScope::Global,
            repository_id: None,
            content: "old rule",
            source: MemorySource::PrLearning,
            source_url: None,
            created_by: None,
            status: MemoryStatus::Active,
            kind: MemoryKind::Convention,
            context: None,
            reason: None,
            resolution: None,
            evidence: &[],
            supersedes: None,
        },
    )
    .await
    .unwrap();
    let evidence = [memories::Evidence {
        url: "https://example.com/pr/12".into(),
        pr_number: Some(12),
    }];
    let replacement = memories::insert(
        &pool,
        NewMemory {
            scope: KnowledgeScope::Global,
            repository_id: None,
            content: "new rule",
            source: MemorySource::PrLearning,
            source_url: None,
            created_by: None,
            status: MemoryStatus::Pending,
            kind: MemoryKind::Convention,
            context: Some("updated context"),
            reason: None,
            resolution: None,
            evidence: &evidence,
            supersedes: Some(target.id),
        },
    )
    .await
    .unwrap();

    let app = app(pool).await;
    let admin = setup_admin(&app).await;

    let approved = post(
        &app,
        &format!("/api/v1/memories/{}/approve", replacement.id),
        &json!({}),
        &admin,
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK);
    assert_eq!(approved.body["status"], "active");
    assert_eq!(approved.body["supersedes"], target.id.to_string());
    assert_eq!(approved.body["evidence"][0]["pr_number"], 12);

    let list = send(&app, get("/api/v1/memories", Some(&admin))).await;
    let old = list
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == target.id.to_string())
        .unwrap();
    assert_eq!(old["superseded_by"], replacement.id.to_string());
}
