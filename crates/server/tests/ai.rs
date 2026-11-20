mod common;

use adus_git::{RemoteRepo, RepoRef};
use axum::{Router, http::StatusCode};
use common::{app, app_and_git, cookie_pair, get, json_req, post_json, send, test_cipher};
use serde_json::{Value, json};
use sqlx::PgPool;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const PASSWORD: &str = "correct horse";
const SECRET: &str = "sk-secret-abcd1234";

async fn setup_admin(app: &Router) -> String {
    let body = json!({ "email": "admin@example.com", "password": PASSWORD });
    let reply = send(app, post_json("/api/v1/auth/setup", &body, None)).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    cookie_pair(&reply)
}

async fn create_provider(app: &Router, admin: &str, body: Value) -> common::Reply {
    send(app, post_json("/api/v1/ai-providers", &body, Some(admin))).await
}

/// Provider `openai` dengan base URL bawaan; mengembalikan id-nya.
async fn openai_provider(app: &Router, admin: &str) -> String {
    let reply = create_provider(app, admin, json!({ "kind": "openai", "name": "OpenAI" })).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    reply.body["id"].as_str().unwrap().to_string()
}

async fn add_key(app: &Router, admin: &str, provider: &str, secret: &str) -> common::Reply {
    let uri = format!("/api/v1/ai-providers/{provider}/keys");
    let body = json!({ "label": "main", "key": secret });
    send(app, post_json(&uri, &body, Some(admin))).await
}

async fn call(app: &Router, method: &str, uri: &str, body: &Value, cookie: &str) -> common::Reply {
    send(app, json_req(method, uri, body, Some(cookie))).await
}

#[sqlx::test(migrations = "../../migrations")]
async fn key_never_returned(pool: PgPool) {
    let app = app(pool.clone()).await;
    let admin = setup_admin(&app).await;
    let provider = openai_provider(&app, &admin).await;

    let created = add_key(&app, &admin, &provider, SECRET).await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.body["key_last4"], "1234");
    assert_eq!(created.body["status"], "ok");
    let key_id = created.body["id"].as_str().unwrap();

    let patched = call(
        &app,
        "PATCH",
        &format!("/api/v1/ai-keys/{key_id}"),
        &json!({ "label": "renamed" }),
        &admin,
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK);

    let list = send(&app, get("/api/v1/ai-providers", Some(&admin))).await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(list.body[0]["keys"][0]["label"], "renamed");

    for body in [&created.body, &patched.body, &list.body] {
        let text = body.to_string();
        assert!(!text.contains(SECRET), "secret leaked: {text}");
        assert!(
            !text.contains("encrypted_key"),
            "encrypted_key leaked: {text}"
        );
    }
    // Yang tersimpan terenkripsi, bukan teks asli.
    let stored: String = sqlx::query_scalar("SELECT encrypted_key FROM ai_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_ne!(stored, SECRET);
    assert_eq!(test_cipher().decrypt(&stored).unwrap(), SECRET);
}

#[sqlx::test(migrations = "../../migrations")]
async fn compatible_without_base_url_422(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;

    for body in [
        json!({ "kind": "openai_compatible", "name": "Local" }),
        json!({ "kind": "openai_compatible", "name": "Local", "base_url": "  " }),
        json!({ "kind": "openai_compatible", "name": "Local", "base_url": "ftp://x" }),
        json!({ "kind": "openai", "name": "OpenAI", "base_url": "not a url" }),
        json!({ "kind": "openai", "name": "  " }),
    ] {
        let reply = create_provider(&app, &admin, body.clone()).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(reply.body["error"]["code"], "validation_error");
    }

    let ok = json!({ "kind": "openai_compatible", "name": "Local", "base_url": "http://localhost:11434/v1" });
    let created = create_provider(&app, &admin, ok).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.body["id"].as_str().unwrap();

    // Base URL tidak boleh dikosongkan pada provider compatible.
    let uri = format!("/api/v1/ai-providers/{id}");
    let clear = call(&app, "PATCH", &uri, &json!({ "base_url": null }), &admin).await;
    assert_eq!(clear.status, StatusCode::UNPROCESSABLE_ENTITY);
    let rename = call(
        &app,
        "PATCH",
        &uri,
        &json!({ "name": "Renamed", "enabled": false }),
        &admin,
    )
    .await;
    assert_eq!(rename.status, StatusCode::OK);
    assert_eq!(rename.body["name"], "Renamed");
    assert_eq!(rename.body["enabled"], false);
    assert_eq!(rename.body["base_url"], "http://localhost:11434/v1");
}

/// Provider compatible yang menunjuk ke server wiremock, dengan satu key.
async fn mock_provider(app: &Router, admin: &str, server: &MockServer) -> (String, String) {
    let body = json!({ "kind": "openai_compatible", "name": "Mock", "base_url": server.uri() });
    let provider = create_provider(app, admin, body).await.body["id"]
        .as_str()
        .unwrap()
        .to_string();
    let key = add_key(app, admin, &provider, SECRET).await.body["id"]
        .as_str()
        .unwrap()
        .to_string();
    (provider, key)
}

async fn test_key(app: &Router, admin: &str, key: &str) -> common::Reply {
    let uri = format!("/api/v1/ai-keys/{key}/test");
    call(app, "POST", &uri, &json!({ "model": "gpt-test" }), admin).await
}

#[sqlx::test(migrations = "../../migrations")]
async fn test_key_reports_failure_without_marking_invalid(pool: PgPool) {
    let app = app(pool.clone()).await;
    let admin = setup_admin(&app).await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let (_provider, key) = mock_provider(&app, &admin, &server).await;

    let reply = test_key(&app, &admin, &key).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["ok"], false);
    assert_eq!(reply.body["error"], "unauthorized");
    assert!(!reply.body.to_string().contains(SECRET));

    // Status key tidak berubah, tetapi percobaan tercatat sebagai usage gagal.
    let status: String = sqlx::query_scalar("SELECT status::text FROM ai_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "ok");
    let row: (
        bool,
        Option<String>,
        Option<uuid::Uuid>,
        Option<uuid::Uuid>,
        String,
    ) = sqlx::query_as(
        "SELECT success, error_kind, review_id, repository_id, model FROM llm_usage",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            false,
            Some("unauthorized".into()),
            None,
            None,
            "gpt-test".into()
        )
    );

    // 429 juga tidak menaruh key ke cooldown.
    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    let limited = test_key(&app, &admin, &key).await;
    assert_eq!(limited.body["ok"], false);
    assert_eq!(limited.body["error"], "rate limited");
    let (status, cooldown): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT status::text, cooldown_until FROM ai_keys")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), cooldown), ("ok", None));
}

#[sqlx::test(migrations = "../../migrations")]
async fn test_key_success_sends_ping_and_records_tokens(pool: PgPool) {
    let app = app(pool.clone()).await;
    let admin = setup_admin(&app).await;
    let server = MockServer::start().await;
    let completion = json!({
        "choices": [{ "message": { "content": "pong" } }],
        "usage": { "prompt_tokens": 7, "completion_tokens": 2 }
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion))
        .mount(&server)
        .await;
    let (_provider, key) = mock_provider(&app, &admin, &server).await;

    let reply = test_key(&app, &admin, &key).await;
    assert_eq!(reply.body, json!({ "ok": true }));

    let sent = &server.received_requests().await.unwrap()[0];
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["model"], "gpt-test");
    assert_eq!(body["max_tokens"], 5);
    assert_eq!(body["messages"][1]["content"], "ping");

    let row: (bool, i32, i32) =
        sqlx::query_as("SELECT success, input_tokens, output_tokens FROM llm_usage")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row, (true, 7, 2));

    // Model kosong ditolak sebelum memanggil provider.
    let uri = format!("/api/v1/ai-keys/{key}/test");
    let bad = call(&app, "POST", &uri, &json!({ "model": " " }), &admin).await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../../migrations")]
async fn replacing_key_restores_status_and_validates(pool: PgPool) {
    let app = app(pool.clone()).await;
    let admin = setup_admin(&app).await;
    let provider = openai_provider(&app, &admin).await;
    let key = add_key(&app, &admin, &provider, SECRET).await.body["id"]
        .as_str()
        .unwrap()
        .to_string();
    sqlx::query(
        "UPDATE ai_keys SET status = 'invalid', cooldown_until = now() + interval '1 hour'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let uri = format!("/api/v1/ai-keys/{key}");
    // Hanya ganti label: status tetap invalid.
    let renamed = call(&app, "PATCH", &uri, &json!({ "label": "x" }), &admin).await;
    assert_eq!(renamed.body["status"], "invalid");

    for bad in [json!({ "key": "  " }), json!({ "label": "" })] {
        let reply = call(&app, "PATCH", &uri, &bad, &admin).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    let fixed = call(
        &app,
        "PATCH",
        &uri,
        &json!({ "key": "sk-new-9999" }),
        &admin,
    )
    .await;
    assert_eq!(fixed.status, StatusCode::OK);
    assert_eq!(fixed.body["status"], "ok");
    assert_eq!(fixed.body["cooldown_until"], Value::Null);
    assert_eq!(fixed.body["key_last4"], "9999");

    let off = call(&app, "PATCH", &uri, &json!({ "enabled": false }), &admin).await;
    assert_eq!(off.body["enabled"], false);

    let gone = call(&app, "DELETE", &uri, &Value::Null, &admin).await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT);
    let again = call(&app, "DELETE", &uri, &Value::Null, &admin).await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../../migrations")]
async fn delete_provider_in_use_409(pool: PgPool) {
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
    let by_settings = openai_provider(&app, &admin).await;
    let by_repo = openai_provider(&app, &admin).await;
    let unused = openai_provider(&app, &admin).await;
    add_key(&app, &admin, &unused, SECRET).await;

    let put = json!({
        "default_provider_id": by_settings, "default_model": "gpt-x",
        "max_files_per_review": 50, "max_patch_lines_per_file": 2000
    });
    assert_eq!(
        call(&app, "PUT", "/api/v1/settings", &put, &admin)
            .await
            .status,
        StatusCode::OK
    );

    let conn =
        json!({ "kind": "github", "name": "Acme", "org_or_owner": "acme", "token": "ghp_x1111" });
    let conn = send(&app, post_json("/api/v1/connections", &conn, Some(&admin))).await;
    let add = json!({ "connection_id": conn.body["id"], "external_id": "1" });
    let repo = send(&app, post_json("/api/v1/repositories", &add, Some(&admin))).await;
    let repo_uri = format!("/api/v1/repositories/{}", repo.body["id"].as_str().unwrap());
    let set = json!({ "review_provider_id": by_repo, "review_model": "gpt-y" });
    assert_eq!(
        call(&app, "PATCH", &repo_uri, &set, &admin).await.status,
        StatusCode::OK
    );

    for id in [&by_settings, &by_repo] {
        let reply = call(
            &app,
            "DELETE",
            &format!("/api/v1/ai-providers/{id}"),
            &Value::Null,
            &admin,
        )
        .await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(reply.body["error"]["code"], "provider_in_use");
    }

    // Repo dilepas -> provider bebas dihapus; tidak terpakai langsung bisa dihapus beserta key-nya.
    assert_eq!(
        call(&app, "DELETE", &repo_uri, &Value::Null, &admin)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    for id in [&by_repo, &unused] {
        let reply = call(
            &app,
            "DELETE",
            &format!("/api/v1/ai-providers/{id}"),
            &Value::Null,
            &admin,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
    }
    let keys: i64 = sqlx::query_scalar("SELECT count(*) FROM ai_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(keys, 0);
    let missing = call(
        &app,
        "DELETE",
        &format!("/api/v1/ai-providers/{unused}"),
        &Value::Null,
        &admin,
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../../migrations")]
async fn settings_roundtrip_and_validation(pool: PgPool) {
    let app = app(pool).await;
    let admin = setup_admin(&app).await;
    let provider = openai_provider(&app, &admin).await;

    let initial = send(&app, get("/api/v1/settings", Some(&admin))).await;
    assert_eq!(initial.status, StatusCode::OK);
    assert_eq!(initial.body["default_provider_id"], Value::Null);
    assert_eq!(initial.body["max_files_per_review"], 50);
    assert_eq!(initial.body["max_patch_lines_per_file"], 2000);

    let put = |provider: Value, model: Value, files: i64, lines: i64| {
        json!({
            "default_provider_id": provider, "default_model": model,
            "max_files_per_review": files, "max_patch_lines_per_file": lines
        })
    };
    let missing_provider = "00000000-0000-0000-0000-000000000001";
    let cases = [
        (
            put(json!(provider), Value::Null, 50, 2000),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            put(json!(provider), json!("  "), 50, 2000),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            put(Value::Null, Value::Null, 0, 2000),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            put(Value::Null, Value::Null, 501, 2000),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            put(Value::Null, Value::Null, 50, 0),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            put(Value::Null, Value::Null, 50, 100_001),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            put(json!(missing_provider), json!("m"), 50, 2000),
            StatusCode::NOT_FOUND,
        ),
    ];
    for (body, status) in cases {
        let reply = call(&app, "PUT", "/api/v1/settings", &body, &admin).await;
        assert_eq!(reply.status, status, "{body}");
    }
    // Semua penolakan tadi tidak mengubah apa pun.
    let unchanged = send(&app, get("/api/v1/settings", Some(&admin))).await;
    assert_eq!(unchanged.body, initial.body);

    let ok = put(json!(provider), json!(" gpt-4o "), 500, 100_000);
    let saved = call(&app, "PUT", "/api/v1/settings", &ok, &admin).await;
    assert_eq!(saved.status, StatusCode::OK);
    assert_eq!(saved.body["default_model"], "gpt-4o");
    assert_eq!(saved.body["default_provider_id"], json!(provider));
    let read = send(&app, get("/api/v1/settings", Some(&admin))).await;
    assert_eq!(read.body, saved.body);
}
