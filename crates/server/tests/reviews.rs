mod common;

use adus_core::types::ConnectionKind;
use adus_db::{
    connections,
    repositories::{self, NewRepository},
};
use axum::{Router, http::StatusCode};
use common::{app, cookie_pair, get, post_json, send};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

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

/// Repo langsung lewat DB. GitHub: `project` kosong; Azure: `owner` = org, `project` = GUID.
async fn add_repo(
    pool: &PgPool,
    kind: ConnectionKind,
    owner: &str,
    project: Option<&str>,
    name: &str,
) -> Uuid {
    let conn = connections::insert(pool, kind, "c", owner, "enc", "1234", "me")
        .await
        .unwrap();
    repositories::insert(
        pool,
        NewRepository {
            connection_id: conn.id,
            external_id: name,
            owner,
            project,
            project_name: None,
            name,
            default_branch: None,
            webhook_token: "t",
        },
    )
    .await
    .unwrap()
    .unwrap()
    .id
}

/// Review dengan `started_at` eksplisit supaya urutan deterministik.
async fn add_review(
    pool: &PgPool,
    repo: Uuid,
    pr: i64,
    status: &str,
    started_at: Option<&str>,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO reviews (repository_id, pr_number, trigger, status, started_at) \
         VALUES ($1, $2, 'opened', $3::review_status, $4::timestamptz) RETURNING id",
    )
    .bind(repo)
    .bind(pr)
    .bind(status)
    .bind(started_at)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn add_usage(pool: &PgPool, review: Uuid, input: i32, output: i32) {
    sqlx::query(
        "INSERT INTO llm_usage (review_id, model, input_tokens, output_tokens, latency_ms, success) \
         VALUES ($1, 'gpt-x', $2, $3, 10, true)",
    )
    .bind(review)
    .bind(input)
    .bind(output)
    .execute(pool)
    .await
    .unwrap();
}

fn ids(body: &Value) -> Vec<String> {
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect()
}

#[sqlx::test(migrations = "../../migrations")]
async fn reviews_list_includes_token_sums_and_pagination(pool: PgPool) {
    let app = app(pool.clone()).await;
    let admin = setup_admin(&app).await;
    let api = add_repo(&pool, ConnectionKind::Github, "acme", None, "api").await;
    let web = add_repo(&pool, ConnectionKind::Github, "acme", None, "web").await;

    let old = add_review(&pool, api, 1, "done", Some("2026-01-01T10:00:00Z")).await;
    let new = add_review(&pool, api, 2, "failed", Some("2026-01-02T10:00:00Z")).await;
    let queued = add_review(&pool, api, 3, "queued", None).await;
    let other = add_review(&pool, web, 9, "done", Some("2026-01-03T10:00:00Z")).await;
    add_usage(&pool, old, 100, 10).await;
    add_usage(&pool, old, 50, 5).await;
    add_usage(&pool, other, 1, 1).await;
    sqlx::query(
        "UPDATE reviews SET error = 'boom', comments_posted = 2, head_sha = 'abc' WHERE id = $1",
    )
    .bind(new)
    .execute(&pool)
    .await
    .unwrap();

    // Semua repo: terbaru dulu. Yang belum mulai diurutkan menurut `created_at` (sekarang),
    // jadi review queued yang baru masuk tampil paling atas, bukan tenggelam di halaman terakhir.
    let reply = send(&app, get("/api/v1/reviews", Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["total"], 4);
    assert_eq!(
        ids(&reply.body),
        [&queued, &other, &new, &old].map(Uuid::to_string)
    );

    // Filter repo + halaman (urutan repo api: queued, new, old).
    let uri = format!("/api/v1/reviews?repository_id={api}&page=3&per_page=1");
    let reply = send(&app, get(&uri, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["total"], 3);
    let item = &reply.body["items"][0];
    assert_eq!(reply.body["items"].as_array().unwrap().len(), 1);
    assert_eq!(item["id"], old.to_string());
    assert_eq!(item["input_tokens"], 150);
    assert_eq!(item["output_tokens"], 15);
    assert_eq!(
        item["repository"],
        json!({ "id": api.to_string(), "name": "acme/api", "kind": "github" })
    );
    assert_eq!(item["pr_number"], 1);
    assert_eq!(item["pr_url"], "https://github.com/acme/api/pull/1");
    assert_eq!(item["trigger"], "opened");
    assert_eq!(item["status"], "done");
    assert_eq!(item["started_at"], "2026-01-01T10:00:00Z");
    assert_eq!(item["finished_at"], Value::Null);

    let uri = format!("/api/v1/reviews?repository_id={api}&page=1&per_page=1");
    let top = &send(&app, get(&uri, Some(&admin))).await.body["items"][0];
    assert_eq!(top["id"], queued.to_string());
    assert_eq!(top["started_at"], Value::Null);

    let uri = format!("/api/v1/reviews?repository_id={api}&page=2&per_page=1");
    let first = &send(&app, get(&uri, Some(&admin))).await.body["items"][0];
    assert_eq!(first["id"], new.to_string());
    assert_eq!(first["error"], "boom");
    assert_eq!(first["comments_posted"], 2);
    assert_eq!(first["head_sha"], "abc");
    assert_eq!(first["skip_reason"], Value::Null);
    // Tanpa baris llm_usage: nol, bukan null.
    assert_eq!(first["input_tokens"], 0);
    assert_eq!(first["output_tokens"], 0);

    // Halaman di luar jangkauan: kosong, total tetap.
    let reply = send(&app, get("/api/v1/reviews?page=9", Some(&admin))).await;
    assert_eq!(reply.body["items"], json!([]));
    assert_eq!(reply.body["total"], 4);
}

#[sqlx::test(migrations = "../../migrations")]
async fn status_filter(pool: PgPool) {
    let app = app(pool.clone()).await;
    let admin = setup_admin(&app).await;
    let repo = add_repo(&pool, ConnectionKind::Github, "acme", None, "api").await;
    add_review(&pool, repo, 1, "done", Some("2026-01-01T10:00:00Z")).await;
    let skipped = add_review(&pool, repo, 2, "skipped", Some("2026-01-02T10:00:00Z")).await;

    let reply = send(&app, get("/api/v1/reviews?status=skipped", Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["total"], 1);
    assert_eq!(ids(&reply.body), [skipped.to_string()]);

    let reply = send(&app, get("/api/v1/reviews?status=nope", Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "../../migrations")]
async fn paging_bounds(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    for q in ["per_page=0", "per_page=101", "page=0"] {
        let reply = send(&app, get(&format!("/api/v1/reviews?{q}"), Some(&admin))).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{q}");
        assert_eq!(reply.body["error"]["code"], "validation_error");
    }
    let reply = send(&app, get("/api/v1/reviews?per_page=100", Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = send(&app, get("/api/v1/reviews?page=x", Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "../../migrations")]
async fn soft_deleted_repo_still_listed_with_pr_url(pool: PgPool) {
    let app = app(pool.clone()).await;
    let admin = setup_admin(&app).await;
    let guid = "6ce954b1-ce1f-45d1-b94d-e6bf2464ba2c";
    let repo = add_repo(
        &pool,
        ConnectionKind::AzureDevops,
        "fabrikam",
        Some(guid),
        "Fab",
    )
    .await;
    add_review(&pool, repo, 5, "done", Some("2026-01-01T10:00:00Z")).await;
    assert!(repositories::soft_delete(&pool, repo).await.unwrap());

    let reply = send(&app, get("/api/v1/reviews", Some(&admin))).await;
    assert_eq!(reply.body["total"], 1);
    let item = &reply.body["items"][0];
    assert_eq!(
        item["repository"],
        json!({ "id": repo.to_string(), "name": "fabrikam/Fab", "kind": "azure_devops" })
    );
    assert_eq!(
        item["pr_url"],
        format!("https://dev.azure.com/fabrikam/{guid}/_git/Fab/pullrequest/5")
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn azure_repo_name_with_space_is_percent_encoded(pool: PgPool) {
    let app = app(pool.clone()).await;
    let admin = setup_admin(&app).await;
    let guid = "6ce954b1-ce1f-45d1-b94d-e6bf2464ba2c";
    let repo = add_repo(
        &pool,
        ConnectionKind::AzureDevops,
        "fabrikam",
        Some(guid),
        "My Repo",
    )
    .await;
    add_review(&pool, repo, 7, "done", Some("2026-01-01T10:00:00Z")).await;

    let reply = send(&app, get("/api/v1/reviews", Some(&admin))).await;
    let item = &reply.body["items"][0];
    assert_eq!(item["repository"]["name"], "fabrikam/My Repo");
    assert_eq!(
        item["pr_url"],
        format!("https://dev.azure.com/fabrikam/{guid}/_git/My%20Repo/pullrequest/7")
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn azure_review_label_uses_project_name_with_org_fallback(pool: PgPool) {
    let app = app(pool.clone()).await;
    let admin = setup_admin(&app).await;
    let guid = "6ce954b1-ce1f-45d1-b94d-e6bf2464ba2c";
    let repo = add_repo(
        &pool,
        ConnectionKind::AzureDevops,
        "ovalteam",
        Some(guid),
        "api",
    )
    .await;
    add_review(&pool, repo, 1, "done", Some("2026-01-01T10:00:00Z")).await;

    let reply = send(&app, get("/api/v1/reviews", Some(&admin))).await;
    assert_eq!(reply.body["items"][0]["repository"]["name"], "ovalteam/api");

    sqlx::query("UPDATE repositories SET project_name = 'myvalue' WHERE id = $1")
        .bind(repo)
        .execute(&pool)
        .await
        .unwrap();
    let reply = send(&app, get("/api/v1/reviews", Some(&admin))).await;
    let item = &reply.body["items"][0];
    assert_eq!(item["repository"]["name"], "myvalue/api");
    // The PR link still uses the organization and project GUID.
    assert_eq!(
        item["pr_url"],
        format!("https://dev.azure.com/ovalteam/{guid}/_git/api/pullrequest/1")
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn user_role_can_read_and_login_required(pool: PgPool) {
    let app = app(pool.clone()).await;
    let admin = setup_admin(&app).await;
    let user = login_as_user(&app, &admin).await;
    let repo = add_repo(&pool, ConnectionKind::Github, "acme", None, "api").await;
    add_review(&pool, repo, 1, "running", Some("2026-01-01T10:00:00Z")).await;

    let reply = send(&app, get("/api/v1/reviews", Some(&user))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["total"], 1);

    let reply = send(&app, get("/api/v1/reviews", None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}
