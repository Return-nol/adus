mod common;

use std::sync::Arc;

use adus_git::{GitError, RemoteRepo, RepoRef};
use axum::{Router, http::StatusCode};
use common::{
    FakeGitFactory, app_and_git, cookie_pair, get, json_req, post_empty, post_json, send,
    test_cipher,
};
use serde_json::{Value, json};
use sqlx::PgPool;

const PASSWORD: &str = "correct horse";
const PAT: &str = "ghp_oldtoken1111";
const NEW_PAT: &str = "ghp_newtoken2222";

async fn setup_admin(app: &Router) -> String {
    let body = json!({ "email": "admin@example.com", "password": PASSWORD });
    let reply = send(app, post_json("/api/v1/auth/setup", &body, None)).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    cookie_pair(&reply)
}

fn github_repo(id: &str, name: &str) -> RemoteRepo {
    RemoteRepo {
        r#ref: RepoRef {
            external_id: id.into(),
            owner: "acme".into(),
            project: None,
            name: name.into(),
        },
        default_branch: Some("main".into()),
        project_name: None,
    }
}

/// App + admin cookie + fake dengan dua repo GitHub yang bisa dilihat.
async fn setup(pool: PgPool) -> (Router, Arc<FakeGitFactory>, String) {
    let (app, git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;
    git.with(|s| s.repos = vec![github_repo("1", "api"), github_repo("2", "web")]);
    (app, git, admin)
}

async fn create_connection(app: &Router, admin: &str, token: &str) -> common::Reply {
    let body = json!({ "kind": "github", "name": "Acme", "org_or_owner": "acme", "token": token });
    send(app, post_json("/api/v1/connections", &body, Some(admin))).await
}

async fn connection_with_token(app: &Router, admin: &str) -> String {
    let reply = create_connection(app, admin, PAT).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    reply.body["id"].as_str().unwrap().to_string()
}

async fn add_repo(app: &Router, admin: &str, conn: &str, external_id: &str) -> common::Reply {
    let body = json!({ "connection_id": conn, "external_id": external_id });
    send(app, post_json("/api/v1/repositories", &body, Some(admin))).await
}

async fn patch_connection(app: &Router, admin: &str, id: &str, body: &Value) -> common::Reply {
    let uri = format!("/api/v1/connections/{id}");
    send(app, json_req("PATCH", &uri, body, Some(admin))).await
}

async fn delete(app: &Router, admin: &str, uri: &str) -> common::Reply {
    send(app, json_req("DELETE", uri, &Value::Null, Some(admin))).await
}

#[sqlx::test(migrations = "../../migrations")]
async fn create_connection_with_bad_token_422_not_saved(pool: PgPool) {
    let (app, git, admin) = setup(pool.clone()).await;
    git.with(|s| s.bad_tokens.insert("bad".into()));

    let reply = create_connection(&app, &admin, "bad").await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "token_invalid");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM connections")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn create_connection_provider_failure_is_502(pool: PgPool) {
    let (app, git, admin) = setup(pool).await;
    git.with(|s| s.verify_error = Some(GitError::Http("boom".into())));

    let reply = create_connection(&app, &admin, PAT).await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
    assert_eq!(reply.body["error"]["code"], "provider_error");
}

#[sqlx::test(migrations = "../../migrations")]
async fn response_never_contains_token(pool: PgPool) {
    let (app, git, admin) = setup(pool).await;
    let created = create_connection(&app, &admin, PAT).await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.body["token_last4"], "1111");
    assert_eq!(created.body["status"], "ok");
    assert_eq!(created.body["pat_owner"], "octocat");
    assert_eq!(created.body["repository_count"], 0);
    let id = created.body["id"].as_str().unwrap().to_string();

    let listed = send(&app, get("/api/v1/connections", Some(&admin))).await;
    let rotated = patch_connection(&app, &admin, &id, &json!({ "token": NEW_PAT })).await;
    git.with(|s| s.bad_tokens.insert("bad".into()));
    let rejected = create_connection(&app, &admin, "bad").await;

    for reply in [&created, &listed, &rotated, &rejected] {
        let text = reply.body.to_string();
        for secret in [PAT, NEW_PAT, "bad"] {
            assert!(!text.contains(secret), "{text} leaks {secret}");
        }
        assert!(!text.contains("encrypted"));
    }
    assert_eq!(rotated.body["token_last4"], "2222");
}

#[sqlx::test(migrations = "../../migrations")]
async fn rotate_token_keeps_repositories(pool: PgPool) {
    let (app, git, admin) = setup(pool.clone()).await;
    let conn = connection_with_token(&app, &admin).await;
    assert_eq!(
        add_repo(&app, &admin, &conn, "1").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        add_repo(&app, &admin, &conn, "2").await.status,
        StatusCode::CREATED
    );
    let before = git.ensure_calls().len();

    let reply = patch_connection(&app, &admin, &conn, &json!({ "token": NEW_PAT })).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["repository_count"], 2);
    assert_eq!(reply.body["webhook_errors"], json!([]));

    // Repo tetap ada, dan webhook diperiksa 2x dengan token BARU.
    let repos = send(&app, get("/api/v1/repositories", Some(&admin))).await;
    assert_eq!(repos.body.as_array().unwrap().len(), 2);
    let calls = git.ensure_calls();
    assert_eq!(calls.len() - before, 2);
    assert!(calls[before..].iter().all(|c| c.token == NEW_PAT));
    // Webhook yang sudah tersimpan diteruskan sebagai `existing`.
    assert!(calls[before..].iter().all(|c| c.existing.len() == 1));

    let removed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repositories WHERE deleted_at IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(removed, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn rotate_with_invalid_token_keeps_old_token(pool: PgPool) {
    let (app, git, admin) = setup(pool.clone()).await;
    let conn = connection_with_token(&app, &admin).await;
    add_repo(&app, &admin, &conn, "1").await;
    git.with(|s| s.bad_tokens.insert(NEW_PAT.into()));
    let ensure_before = git.ensure_calls().len();

    let body = json!({ "name": "Renamed", "token": NEW_PAT });
    let reply = patch_connection(&app, &admin, &conn, &body).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "token_invalid");

    let (encrypted, last4, name, status): (String, String, String, String) =
        sqlx::query_as("SELECT encrypted_token, token_last4, name, status::text FROM connections")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(test_cipher().decrypt(&encrypted).unwrap(), PAT);
    assert_eq!(last4, "1111");
    assert_eq!(name, "Acme");
    assert_eq!(status, "ok");
    assert_eq!(git.ensure_calls().len(), ensure_before);
    let listed = send(&app, get("/api/v1/repositories", Some(&admin))).await;
    assert_eq!(listed.body.as_array().unwrap().len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn rotate_token_webhook_failure_reported_not_fatal(pool: PgPool) {
    let (app, git, admin) = setup(pool.clone()).await;
    let conn = connection_with_token(&app, &admin).await;
    add_repo(&app, &admin, &conn, "1").await;
    let failing = add_repo(&app, &admin, &conn, "2").await;
    let failing_id = failing.body["id"].as_str().unwrap().to_string();
    git.with(|s| {
        s.ensure_errors
            .insert("2".into(), GitError::Http("secret detail".into()))
    });

    let reply = patch_connection(&app, &admin, &conn, &json!({ "token": NEW_PAT })).await;
    assert_eq!(reply.status, StatusCode::OK);
    let errors = reply.body["webhook_errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["repository_id"], failing_id);
    assert!(
        !errors[0]["message"]
            .as_str()
            .unwrap()
            .contains("secret detail")
    );

    // Token baru tersimpan dan kedua repo masih ada.
    let encrypted: String = sqlx::query_scalar("SELECT encrypted_token FROM connections")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(test_cipher().decrypt(&encrypted).unwrap(), NEW_PAT);
    assert_eq!(reply.body["repository_count"], 2);
}

#[sqlx::test(migrations = "../../migrations")]
async fn resync_recreates_missing_webhooks(pool: PgPool) {
    let (app, git, admin) = setup(pool.clone()).await;
    let conn = connection_with_token(&app, &admin).await;
    add_repo(&app, &admin, &conn, "1").await;
    sqlx::query("UPDATE repositories SET webhook_external_ids = '{}'")
        .execute(&pool)
        .await
        .unwrap();

    let uri = format!("/api/v1/connections/{conn}/resync-webhooks");
    let reply = send(&app, post_empty(&uri, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, json!({ "synced": 1, "webhook_errors": [] }));
    assert!(git.ensure_calls().last().unwrap().existing.is_empty());
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT unnest(webhook_external_ids) FROM repositories")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(ids, vec!["hook-1"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn delete_connection_with_repos_409(pool: PgPool) {
    let (app, _git, admin) = setup(pool).await;
    let conn = connection_with_token(&app, &admin).await;
    let repo = add_repo(&app, &admin, &conn, "1").await;
    let repo_id = repo.body["id"].as_str().unwrap().to_string();

    let uri = format!("/api/v1/connections/{conn}");
    let reply = delete(&app, &admin, &uri).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["error"]["code"], "connection_in_use");

    // Setelah repo dihapus (soft), connection boleh dihapus.
    let removed = delete(&app, &admin, &format!("/api/v1/repositories/{repo_id}")).await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    assert_eq!(
        delete(&app, &admin, &uri).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        delete(&app, &admin, &uri).await.status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn add_repo_registers_webhook_and_stores_ids(pool: PgPool) {
    let (app, git, admin) = setup(pool.clone()).await;
    let conn = connection_with_token(&app, &admin).await;

    let reply = add_repo(&app, &admin, &conn, "1").await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.body["name"], "api");
    assert_eq!(reply.body["owner"], "acme");
    assert_eq!(reply.body["webhook_count"], 1);
    assert!(reply.body.get("webhook_token").is_none());
    let id = reply.body["id"].as_str().unwrap();

    let calls = git.ensure_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].url,
        format!("http://localhost:8000/webhooks/github/{id}")
    );
    assert!(calls[0].existing.is_empty());

    let (ids, token): (Vec<String>, String) =
        sqlx::query_as("SELECT webhook_external_ids, webhook_token FROM repositories")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ids, vec!["hook-1"]);
    assert_eq!(token.len(), 43); // 32 byte base64url tanpa padding
    assert_eq!(calls[0].secret, token);

    let conns = send(&app, get("/api/v1/connections", Some(&admin))).await;
    assert_eq!(conns.body[0]["repository_count"], 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn azure_repo_webhook_url_carries_token(pool: PgPool) {
    let (app, git, admin) = setup(pool.clone()).await;
    git.with(|s| {
        s.repos = vec![RemoteRepo {
            r#ref: RepoRef {
                external_id: "repo-guid".into(),
                owner: "myorg".into(),
                project: Some("project-guid".into()),
                name: "svc".into(),
            },
            default_branch: None,
            project_name: None,
        }]
    });
    let body =
        json!({ "kind": "azure_devops", "name": "Az", "org_or_owner": "myorg", "token": PAT });
    let conn = send(&app, post_json("/api/v1/connections", &body, Some(&admin))).await;
    let conn = conn.body["id"].as_str().unwrap().to_string();

    let reply = add_repo(&app, &admin, &conn, "repo-guid").await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.body["project"], "project-guid");

    let token: String = sqlx::query_scalar("SELECT webhook_token FROM repositories")
        .fetch_one(&pool)
        .await
        .unwrap();
    let id = reply.body["id"].as_str().unwrap();
    assert_eq!(
        git.ensure_calls()[0].url,
        format!("http://localhost:8000/webhooks/azure/{id}?token={token}")
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn add_repo_webhook_failure_502_and_not_stored(pool: PgPool) {
    let (app, git, admin) = setup(pool.clone()).await;
    let conn = connection_with_token(&app, &admin).await;
    git.with(|s| {
        s.ensure_errors
            .insert("1".into(), GitError::Http("nope".into()))
    });

    let reply = add_repo(&app, &admin, &conn, "1").await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
    assert_eq!(reply.body["error"]["code"], "webhook_failed");
    // Test memakai http://localhost:8000: provider tidak bisa menjangkaunya.
    let message = reply.body["error"]["message"].as_str().unwrap();
    assert!(
        message.ends_with(
            " — GitHub/Azure must be able to reach ADUS_PUBLIC_URL (currently not public)"
        ),
        "{message}"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM repositories")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    // Setelah provider pulih, repo yang sama bisa ditambahkan.
    git.with(|s| s.ensure_errors.clear());
    assert_eq!(
        add_repo(&app, &admin, &conn, "1").await.status,
        StatusCode::CREATED
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn add_repo_twice_409_but_readd_after_delete_ok(pool: PgPool) {
    let (app, git, admin) = setup(pool.clone()).await;
    let conn = connection_with_token(&app, &admin).await;
    let first = add_repo(&app, &admin, &conn, "1").await;
    let calls = git.ensure_calls().len();

    let again = add_repo(&app, &admin, &conn, "1").await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.body["error"]["code"], "repository_exists");
    assert_eq!(git.ensure_calls().len(), calls);

    let id = first.body["id"].as_str().unwrap();
    delete(&app, &admin, &format!("/api/v1/repositories/{id}")).await;
    let fresh = add_repo(&app, &admin, &conn, "1").await;
    assert_eq!(fresh.status, StatusCode::CREATED);
    assert_ne!(fresh.body["id"], first.body["id"]);

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM repositories")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 2); // satu soft-deleted + satu aktif
}

#[sqlx::test(migrations = "../../migrations")]
async fn add_unknown_remote_repo_404(pool: PgPool) {
    let (app, _git, admin) = setup(pool).await;
    let conn = connection_with_token(&app, &admin).await;
    let reply = add_repo(&app, &admin, &conn, "999").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body["error"]["code"], "remote_repo_not_found");
}

#[sqlx::test(migrations = "../../migrations")]
async fn delete_repo_soft_deletes_even_if_provider_delete_fails(pool: PgPool) {
    let (app, git, admin) = setup(pool.clone()).await;
    let conn = connection_with_token(&app, &admin).await;
    let repo = add_repo(&app, &admin, &conn, "1").await;
    let id = repo.body["id"].as_str().unwrap().to_string();
    git.with(|s| s.delete_error = Some(GitError::Unauthorized));

    let reply = delete(&app, &admin, &format!("/api/v1/repositories/{id}")).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);

    assert_eq!(
        git.delete_calls(),
        vec![(PAT.to_string(), "1".to_string(), vec!["hook-1".to_string()])]
    );
    let deleted: bool = sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM repositories")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(deleted);
    let listed = send(&app, get("/api/v1/repositories", Some(&admin))).await;
    assert_eq!(listed.body, json!([]));
    let conns = send(&app, get("/api/v1/connections", Some(&admin))).await;
    assert_eq!(conns.body[0]["repository_count"], 0);
    let again = delete(&app, &admin, &format!("/api/v1/repositories/{id}")).await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../../migrations")]
async fn remote_repos_flags_added_and_marks_invalid_on_bad_token(pool: PgPool) {
    let (app, git, admin) = setup(pool).await;
    let conn = connection_with_token(&app, &admin).await;
    add_repo(&app, &admin, &conn, "1").await;

    let uri = format!("/api/v1/connections/{conn}/remote-repos");
    let reply = send(&app, get(&uri, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body[0]["ref"]["external_id"], "1");
    assert_eq!(reply.body[0]["added"], true);
    assert_eq!(reply.body[1]["added"], false);

    git.with(|s| s.bad_tokens.insert(PAT.into()));
    let reply = send(&app, get(&uri, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "token_invalid");
    let conns = send(&app, get("/api/v1/connections", Some(&admin))).await;
    assert_eq!(conns.body[0]["status"], "invalid");
    assert_eq!(conns.body[0]["repository_count"], 1);

    // Token diganti ke yang valid: status kembali ok, repo tidak hilang.
    let fixed = patch_connection(&app, &admin, &conn, &json!({ "token": NEW_PAT })).await;
    assert_eq!(fixed.body["status"], "ok");
    assert_eq!(fixed.body["repository_count"], 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn update_repository_settings(pool: PgPool) {
    let (app, _git, admin) = setup(pool).await;
    let provider = send(
        &app,
        post_json(
            "/api/v1/ai-providers",
            &json!({ "kind": "openai", "name": "OpenAI", "primary_model": "gpt-5" }),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(provider.status, StatusCode::CREATED);
    let provider_id = provider.body["id"].as_str().unwrap();

    let conn = connection_with_token(&app, &admin).await;
    let repo = add_repo(&app, &admin, &conn, "1").await;
    let uri = format!("/api/v1/repositories/{}", repo.body["id"].as_str().unwrap());

    let body = json!({ "enabled": false, "review_provider_id": provider_id });
    let reply = send(&app, json_req("PATCH", &uri, &body, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["enabled"], false);
    assert_eq!(reply.body["auto_review"], true);
    assert_eq!(reply.body["review_provider_id"], provider_id);

    let missing = send(
        &app,
        json_req(
            "PATCH",
            &uri,
            &json!({ "review_provider_id": "00000000-0000-0000-0000-000000000001" }),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    // Field yang tidak dikirim tetap; `null` mengosongkan.
    let body = json!({ "review_provider_id": null });
    let reply = send(&app, json_req("PATCH", &uri, &body, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["review_provider_id"], Value::Null);
    assert_eq!(reply.body["enabled"], false);
}

#[sqlx::test(migrations = "../../migrations")]
async fn list_repositories_filters_by_connection(pool: PgPool) {
    let (app, _git, admin) = setup(pool).await;
    let first = connection_with_token(&app, &admin).await;
    let second = connection_with_token(&app, &admin).await;
    add_repo(&app, &admin, &first, "1").await;
    add_repo(&app, &admin, &second, "1").await;

    let all = send(&app, get("/api/v1/repositories", Some(&admin))).await;
    assert_eq!(all.body.as_array().unwrap().len(), 2);
    let uri = format!("/api/v1/repositories?connection_id={second}");
    let one = send(&app, get(&uri, Some(&admin))).await;
    assert_eq!(one.body.as_array().unwrap().len(), 1);
    assert_eq!(one.body[0]["connection_id"], second);

    let uri = "/api/v1/repositories?connection_id=nope";
    let bad = send(&app, get(uri, Some(&admin))).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad.body["error"]["code"], "bad_request");
}

#[sqlx::test(migrations = "../../migrations")]
async fn user_role_cannot_create_connection_403(pool: PgPool) {
    let (app, _git, admin) = setup(pool).await;
    let conn = connection_with_token(&app, &admin).await;
    let body = json!({ "email": "u@example.com", "password": PASSWORD, "role": "user" });
    send(&app, post_json("/api/v1/users", &body, Some(&admin))).await;
    let login = json!({ "email": "u@example.com", "password": PASSWORD });
    let user = cookie_pair(&send(&app, post_json("/api/v1/auth/login", &login, None)).await);

    let reply = create_connection(&app, &user, PAT).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);

    // Users may read, but not change or probe.
    let read = send(&app, get("/api/v1/connections", Some(&user))).await;
    assert_eq!(read.status, StatusCode::OK);
    let read = send(&app, get("/api/v1/repositories", Some(&user))).await;
    assert_eq!(read.status, StatusCode::OK);
    let patch = patch_connection(&app, &user, &conn, &json!({ "name": "x" })).await;
    assert_eq!(patch.status, StatusCode::FORBIDDEN);
    let uri = format!("/api/v1/connections/{conn}/remote-repos");
    let remote = send(&app, get(&uri, Some(&user))).await;
    assert_eq!(remote.status, StatusCode::FORBIDDEN);
    let added = add_repo(&app, &user, &conn, "1").await;
    assert_eq!(added.status, StatusCode::FORBIDDEN);
    let unauth = send(&app, get("/api/v1/connections", None)).await;
    assert_eq!(unauth.status, StatusCode::UNAUTHORIZED);
}

fn azure_repo(project_name: Option<&str>) -> RemoteRepo {
    RemoteRepo {
        r#ref: RepoRef {
            external_id: "repo-guid".into(),
            owner: "ovalteam".into(),
            project: Some("project-guid".into()),
            name: "api".into(),
        },
        default_branch: None,
        project_name: project_name.map(String::from),
    }
}

/// Azure connection plus a repo row that predates `project_name` (NULL).
async fn legacy_azure_repo(
    pool: &PgPool,
    app: &Router,
    admin: &str,
    git: &FakeGitFactory,
) -> String {
    git.with(|s| s.repos = vec![azure_repo(Some("myvalue"))]);
    let body = json!({
        "kind": "azure_devops", "name": "Az", "org_or_owner": "ovalteam", "token": PAT
    });
    let conn = send(app, post_json("/api/v1/connections", &body, Some(admin))).await;
    let conn = conn.body["id"].as_str().unwrap().to_string();
    let reply = add_repo(app, admin, &conn, "repo-guid").await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.body["display_name"], "myvalue/api");
    sqlx::query("UPDATE repositories SET project_name = NULL")
        .execute(pool)
        .await
        .unwrap();

    let list = send(app, get("/api/v1/repositories", Some(admin))).await;
    assert_eq!(list.body[0]["display_name"], "ovalteam/api");
    conn
}

async fn assert_backfilled(pool: &PgPool, app: &Router, admin: &str) {
    let name: Option<String> = sqlx::query_scalar("SELECT project_name FROM repositories")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(name.as_deref(), Some("myvalue"));
    let list = send(app, get("/api/v1/repositories", Some(admin))).await;
    assert_eq!(list.body[0]["project_name"], "myvalue");
    assert_eq!(list.body[0]["display_name"], "myvalue/api");
}

#[sqlx::test(migrations = "../../migrations")]
async fn remote_repos_backfills_project_name(pool: PgPool) {
    let (app, git, admin) = setup(pool.clone()).await;
    let conn = legacy_azure_repo(&pool, &app, &admin, &git).await;

    let remote = send(
        &app,
        get(
            &format!("/api/v1/connections/{conn}/remote-repos"),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(remote.status, StatusCode::OK);
    assert_eq!(remote.body[0]["display_name"], "myvalue/api");
    assert_backfilled(&pool, &app, &admin).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn resync_backfills_project_name(pool: PgPool) {
    let (app, git, admin) = setup(pool.clone()).await;
    let conn = legacy_azure_repo(&pool, &app, &admin, &git).await;

    let reply = send(
        &app,
        post_empty(
            &format!("/api/v1/connections/{conn}/resync-webhooks"),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["synced"], 1);
    assert_backfilled(&pool, &app, &admin).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn github_display_name_is_owner_name(pool: PgPool) {
    let (app, _git, admin) = setup(pool).await;
    let conn = connection_with_token(&app, &admin).await;
    let remote = send(
        &app,
        get(
            &format!("/api/v1/connections/{conn}/remote-repos"),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(remote.body[0]["display_name"], "acme/api");
    let added = add_repo(&app, &admin, &conn, "1").await;
    assert_eq!(added.body["display_name"], "acme/api");
    assert_eq!(added.body["project_name"], Value::Null);
}

#[sqlx::test(migrations = "../../migrations")]
async fn remote_repos_display_name_falls_back_to_org_without_project_name(pool: PgPool) {
    let (app, git, admin) = setup(pool).await;
    git.with(|s| s.repos = vec![azure_repo(None)]);
    let body = json!({
        "kind": "azure_devops", "name": "Az", "org_or_owner": "ovalteam", "token": PAT
    });
    let conn = send(&app, post_json("/api/v1/connections", &body, Some(&admin))).await;
    let conn = conn.body["id"].as_str().unwrap();

    let remote = send(
        &app,
        get(
            &format!("/api/v1/connections/{conn}/remote-repos"),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(remote.status, StatusCode::OK);
    assert_eq!(remote.body[0]["display_name"], "ovalteam/api");
    assert_eq!(remote.body[0]["added"], false);
}

#[sqlx::test(migrations = "../../migrations")]
async fn resync_continues_when_list_repos_fails(pool: PgPool) {
    let (app, git, admin) = setup(pool.clone()).await;
    let conn = legacy_azure_repo(&pool, &app, &admin, &git).await;
    git.with(|s| s.list_error = Some(GitError::Http("down".into())));

    let reply = send(
        &app,
        post_empty(
            &format!("/api/v1/connections/{conn}/resync-webhooks"),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["synced"], 1);
    let name: Option<String> = sqlx::query_scalar("SELECT project_name FROM repositories")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, None);
}

#[sqlx::test(migrations = "../../migrations")]
async fn github_resync_does_not_list_remote_repos(pool: PgPool) {
    let (app, git, admin) = setup(pool).await;
    let conn = connection_with_token(&app, &admin).await;
    assert_eq!(
        add_repo(&app, &admin, &conn, "1").await.status,
        StatusCode::CREATED
    );
    let before = git.with(|s| s.list_calls);

    let reply = send(
        &app,
        post_empty(
            &format!("/api/v1/connections/{conn}/resync-webhooks"),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["synced"], 1);
    assert_eq!(git.with(|s| s.list_calls), before);
}

#[sqlx::test(migrations = "../../migrations")]
async fn repository_learning_mode_round_trip(pool: PgPool) {
    let (app, _git, admin) = setup(pool).await;
    let conn = connection_with_token(&app, &admin).await;
    let repo = add_repo(&app, &admin, &conn, "1").await;
    assert_eq!(repo.body["learning_mode"], "approval");
    let uri = format!("/api/v1/repositories/{}", repo.body["id"].as_str().unwrap());

    let body = json!({ "learning_mode": "auto" });
    let reply = send(&app, json_req("PATCH", &uri, &body, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["learning_mode"], "auto");

    let user_body = json!({ "email": "u@example.com", "password": PASSWORD, "role": "user" });
    send(&app, post_json("/api/v1/users", &user_body, Some(&admin))).await;
    let login = json!({ "email": "u@example.com", "password": PASSWORD });
    let user = cookie_pair(&send(&app, post_json("/api/v1/auth/login", &login, None)).await);
    let body = json!({ "learning_mode": "off" });
    let reply = send(&app, json_req("PATCH", &uri, &body, Some(&user))).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}
