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

async fn call(app: &Router, method: &str, uri: &str, body: &Value, cookie: &str) -> common::Reply {
    send(app, json_req(method, uri, body, Some(cookie))).await
}

/// Provider `openai` dengan satu key berlabel `label`; mengembalikan (provider_id, key_id).
async fn provider_with_key(app: &Router, admin: &str, name: &str, label: &str) -> (String, String) {
    let body = json!({ "kind": "openai", "name": name, "primary_model": "gpt-5" });
    let provider = send(app, post_json("/api/v1/ai-providers", &body, Some(admin))).await;
    let provider = provider.body["id"].as_str().unwrap().to_string();
    let uri = format!("/api/v1/ai-providers/{provider}/keys");
    let body = json!({ "label": label, "key": "sk-abcdef123456" });
    let key = send(app, post_json(&uri, &body, Some(admin))).await;
    (provider, key.body["id"].as_str().unwrap().to_string())
}

struct Row<'a> {
    at: &'a str,
    provider: &'a str,
    key: &'a str,
    model: &'a str,
    input: i32,
    output: i32,
    success: bool,
    repo: Option<&'a str>,
}

async fn insert_usage(pool: &PgPool, row: Row<'_>) {
    sqlx::query(
        "INSERT INTO llm_usage (repository_id, provider_id, key_id, model, input_tokens, \
         output_tokens, latency_ms, success, created_at) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5, $6, 10, $7, $8::timestamptz)",
    )
    .bind(row.repo)
    .bind(row.provider)
    .bind(row.key)
    .bind(row.model)
    .bind(row.input)
    .bind(row.output)
    .bind(row.success)
    .bind(row.at)
    .execute(pool)
    .await
    .unwrap();
}

fn row<'a>(at: &'a str, provider: &'a str, key: &'a str, input: i32, output: i32) -> Row<'a> {
    Row {
        at,
        provider,
        key,
        model: "gpt-x",
        input,
        output,
        success: true,
        repo: None,
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn daily_usage_fills_missing_days_with_zero(pool: PgPool) {
    let (app, _git) = app_and_git(pool.clone()).await;
    let admin = setup_admin(&app).await;
    let (p, k) = provider_with_key(&app, &admin, "OpenAI", "main").await;

    insert_usage(&pool, row("2026-01-10T00:00:00Z", &p, &k, 100, 10)).await;
    let failed = Row {
        success: false,
        ..row("2026-01-10T23:59:59Z", &p, &k, 0, 0)
    };
    insert_usage(&pool, failed).await;
    insert_usage(&pool, row("2026-01-12T12:00:00Z", &p, &k, 5, 1)).await;
    // Di luar rentang (sehari sesudahnya, tepat tengah malam UTC): tidak ikut.
    insert_usage(&pool, row("2026-01-13T00:00:00Z", &p, &k, 999, 999)).await;

    let uri = "/api/v1/usage/daily?from=2026-01-09&to=2026-01-12";
    let reply = send(&app, get(uri, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    let day = |date: &str, i: i64, o: i64, calls: i64, failed: i64| json!({ "date": date, "input_tokens": i, "output_tokens": o, "calls": calls, "failed_calls": failed });
    assert_eq!(
        reply.body,
        json!([
            day("2026-01-09", 0, 0, 0, 0),
            day("2026-01-10", 100, 10, 2, 1),
            day("2026-01-11", 0, 0, 0, 0),
            day("2026-01-12", 5, 1, 1, 0),
        ])
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn daily_range_defaults_and_limits(pool: PgPool) {
    let (app, _git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;
    let daily = |q: &str| get(&format!("/api/v1/usage/daily{q}"), Some(&admin));

    // Default: 30 hari yang berakhir hari ini (UTC).
    let default = send(&app, daily("")).await;
    assert_eq!(default.status, StatusCode::OK);
    let days = default.body.as_array().unwrap();
    assert_eq!(days.len(), 30);
    let today = chrono::Utc::now().date_naive().to_string();
    // Toleransi pergantian hari saat test berjalan.
    let yesterday = (chrono::Utc::now().date_naive() - chrono::Duration::days(1)).to_string();
    let last = days[29]["date"].as_str().unwrap();
    assert!(last == today || last == yesterday, "last day {last}");

    // 366 hari inklusif boleh, 367 tidak; terbalik tidak.
    let max = send(&app, daily("?from=2025-01-01&to=2026-01-01")).await;
    assert_eq!(max.status, StatusCode::OK);
    assert_eq!(max.body.as_array().unwrap().len(), 366);
    for q in [
        "?from=2024-12-31&to=2026-01-01",
        "?from=2026-01-02&to=2026-01-01",
    ] {
        let reply = send(&app, daily(q)).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{q}");
        assert_eq!(reply.body["error"]["code"], "validation_error");
    }
    let garbage = send(&app, daily("?from=yesterday")).await;
    assert_eq!(garbage.status, StatusCode::BAD_REQUEST);
    let anonymous = send(&app, get("/api/v1/usage/daily", None)).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations")]
async fn breakdown_by_key_uses_label(pool: PgPool) {
    let (app, _git) = app_and_git(pool.clone()).await;
    let admin = setup_admin(&app).await;
    let (p1, k1) = provider_with_key(&app, &admin, "OpenAI", "primary").await;
    let (p2, k2) = provider_with_key(&app, &admin, "Backup AI", "backup").await;

    insert_usage(&pool, row("2026-02-01T10:00:00Z", &p1, &k1, 10, 5)).await;
    insert_usage(&pool, row("2026-02-02T10:00:00Z", &p1, &k1, 10, 5)).await;
    insert_usage(&pool, row("2026-02-02T11:00:00Z", &p2, &k2, 500, 100)).await;

    let q = "from=2026-02-01&to=2026-02-28";
    let by_key = send(
        &app,
        get(&format!("/api/v1/usage/breakdown?by=key&{q}"), Some(&admin)),
    )
    .await;
    assert_eq!(by_key.status, StatusCode::OK);
    // Token terbanyak dulu; label = label key, bukan last4.
    assert_eq!(
        by_key.body,
        json!([
            { "id": k2, "label": "backup", "input_tokens": 500, "output_tokens": 100, "calls": 1 },
            { "id": k1, "label": "primary", "input_tokens": 20, "output_tokens": 10, "calls": 2 },
        ])
    );

    let by_provider = send(
        &app,
        get(
            &format!("/api/v1/usage/breakdown?by=provider&{q}"),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(by_provider.body[0]["label"], "Backup AI");
    assert_eq!(by_provider.body[1]["label"], "OpenAI");

    // Model tidak punya id.
    let by_model = send(
        &app,
        get(
            &format!("/api/v1/usage/breakdown?by=model&{q}"),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(
        by_model.body,
        json!([{ "label": "gpt-x", "input_tokens": 520, "output_tokens": 110, "calls": 3 }])
    );

    // Key dihapus: usage-nya bertahan dengan label "(deleted)".
    let del = call(
        &app,
        "DELETE",
        &format!("/api/v1/ai-keys/{k1}"),
        &Value::Null,
        &admin,
    )
    .await;
    assert_eq!(del.status, StatusCode::NO_CONTENT);
    let after = send(
        &app,
        get(&format!("/api/v1/usage/breakdown?by=key&{q}"), Some(&admin)),
    )
    .await;
    assert_eq!(
        after.body,
        json!([
            { "id": k2, "label": "backup", "input_tokens": 500, "output_tokens": 100, "calls": 1 },
            { "label": "(deleted)", "input_tokens": 20, "output_tokens": 10, "calls": 2 },
        ])
    );

    let bad = send(&app, get("/api/v1/usage/breakdown?by=color", Some(&admin))).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let missing = send(&app, get("/api/v1/usage/breakdown", Some(&admin))).await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "../../migrations")]
async fn breakdown_by_repository_uses_owner_name(pool: PgPool) {
    let (app, git) = app_and_git(pool.clone()).await;
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
    let conn =
        json!({ "kind": "github", "name": "Acme", "org_or_owner": "acme", "token": "ghp_x1111" });
    let conn = send(&app, post_json("/api/v1/connections", &conn, Some(&admin))).await;
    let add = json!({ "connection_id": conn.body["id"], "external_id": "1" });
    let repo = send(&app, post_json("/api/v1/repositories", &add, Some(&admin))).await;
    let repo_id = repo.body["id"].as_str().unwrap().to_string();
    let (p, k) = provider_with_key(&app, &admin, "OpenAI", "main").await;

    let with_repo = Row {
        repo: Some(&repo_id),
        ..row("2026-03-01T10:00:00Z", &p, &k, 30, 3)
    };
    insert_usage(&pool, with_repo).await;
    // Tanpa repo (mis. tes key): digabung ke "(deleted)".
    insert_usage(&pool, row("2026-03-01T11:00:00Z", &p, &k, 1, 1)).await;

    let uri = "/api/v1/usage/breakdown?by=repository&from=2026-03-01&to=2026-03-01";
    let reply = send(&app, get(uri, Some(&admin))).await;
    assert_eq!(
        reply.body,
        json!([
            { "id": repo_id, "label": "acme/api", "input_tokens": 30, "output_tokens": 3, "calls": 1 },
            { "label": "(deleted)", "input_tokens": 1, "output_tokens": 1, "calls": 1 },
        ])
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn breakdown_by_repository_uses_azure_project_name(pool: PgPool) {
    let (app, git) = app_and_git(pool.clone()).await;
    let admin = setup_admin(&app).await;
    git.with(|s| {
        s.repos = vec![RemoteRepo {
            r#ref: RepoRef {
                external_id: "1".into(),
                owner: "ovalteam".into(),
                project: Some("project-guid".into()),
                name: "api".into(),
            },
            default_branch: None,
            project_name: Some("myvalue".into()),
        }]
    });
    let conn = json!({
        "kind": "azure_devops", "name": "Az", "org_or_owner": "ovalteam", "token": "pat-x1111"
    });
    let conn = send(&app, post_json("/api/v1/connections", &conn, Some(&admin))).await;
    let add = json!({ "connection_id": conn.body["id"], "external_id": "1" });
    let repo = send(&app, post_json("/api/v1/repositories", &add, Some(&admin))).await;
    let repo_id = repo.body["id"].as_str().unwrap().to_string();
    let (p, k) = provider_with_key(&app, &admin, "OpenAI", "main").await;

    let with_repo = Row {
        repo: Some(&repo_id),
        ..row("2026-03-01T10:00:00Z", &p, &k, 30, 3)
    };
    insert_usage(&pool, with_repo).await;

    let uri = "/api/v1/usage/breakdown?by=repository&from=2026-03-01&to=2026-03-01";
    let reply = send(&app, get(uri, Some(&admin))).await;
    assert_eq!(
        reply.body,
        json!([{ "id": repo_id, "label": "myvalue/api", "input_tokens": 30, "output_tokens": 3, "calls": 1 }])
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn user_role_can_read_usage_but_not_edit_keys(pool: PgPool) {
    let (app, _git) = app_and_git(pool).await;
    let admin = setup_admin(&app).await;
    let (provider, key) = provider_with_key(&app, &admin, "OpenAI", "main").await;
    let body = json!({ "email": "u@example.com", "password": PASSWORD, "role": "user" });
    send(&app, post_json("/api/v1/users", &body, Some(&admin))).await;
    let login = json!({ "email": "u@example.com", "password": PASSWORD });
    let user = cookie_pair(&send(&app, post_json("/api/v1/auth/login", &login, None)).await);

    for uri in [
        "/api/v1/usage/daily",
        "/api/v1/usage/breakdown?by=model",
        "/api/v1/ai-providers",
        "/api/v1/settings",
    ] {
        let reply = send(&app, get(uri, Some(&user))).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
    }
    // Pengguna biasa melihat key (tanpa rahasia) tetapi tidak bisa mengubah apa pun.
    let list = send(&app, get("/api/v1/ai-providers", Some(&user))).await;
    assert_eq!(list.body[0]["keys"][0]["key_last4"], "3456");

    let key_uri = format!("/api/v1/ai-keys/{key}");
    let provider_uri = format!("/api/v1/ai-providers/{provider}");
    let attempts = [
        (
            "POST",
            "/api/v1/ai-providers".to_string(),
            json!({ "kind": "openai", "name": "x", "primary_model": "gpt-5" }),
        ),
        ("PATCH", provider_uri.clone(), json!({ "name": "x" })),
        ("DELETE", provider_uri.clone(), Value::Null),
        (
            "POST",
            format!("{provider_uri}/keys"),
            json!({ "label": "x", "key": "y" }),
        ),
        ("PATCH", key_uri.clone(), json!({ "key": "sk-stolen" })),
        ("DELETE", key_uri.clone(), Value::Null),
        ("POST", format!("{key_uri}/test"), json!({ "model": "m" })),
        (
            "PUT",
            "/api/v1/settings".to_string(),
            json!({ "default_provider_id": null,
                    "max_files_per_review": 1, "max_patch_lines_per_file": 1 }),
        ),
    ];
    for (method, uri, body) in attempts {
        let reply = call(&app, method, &uri, &body, &user).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(reply.body["error"]["code"], "forbidden");
    }
    // Tidak ada yang berubah.
    let list = send(&app, get("/api/v1/ai-providers", Some(&admin))).await;
    assert_eq!(list.body[0]["name"], "OpenAI");
    assert_eq!(list.body[0]["keys"][0]["key_last4"], "3456");
}
