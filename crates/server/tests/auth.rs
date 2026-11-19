mod common;

use axum::http::StatusCode;
use common::{app, app_with_url, cookie_pair, get, post_empty, post_json, send, set_cookie};
use serde_json::json;
use sqlx::PgPool;

fn creds(email: &str, password: &str) -> serde_json::Value {
    json!({ "email": email, "password": password })
}

#[sqlx::test(migrations = "../../migrations")]
async fn status_needs_setup_when_no_users(pool: PgPool) {
    let app = app(pool);
    let reply = send(&app, get("/api/v1/auth/status", None)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, json!({ "needs_setup": true }));

    let body = creds("admin@example.com", "correct horse");
    send(&app, post_json("/api/v1/auth/setup", &body, None)).await;
    let reply = send(&app, get("/api/v1/auth/status", None)).await;
    assert_eq!(reply.body, json!({ "needs_setup": false }));
}

#[sqlx::test(migrations = "../../migrations")]
async fn setup_creates_admin_and_sets_cookie(pool: PgPool) {
    let app = app(pool);
    let body = creds("  admin@example.com ", "correct horse");
    let reply = send(&app, post_json("/api/v1/auth/setup", &body, None)).await;

    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.body["email"], "admin@example.com");
    assert_eq!(reply.body["role"], "admin");
    assert_eq!(reply.body["disabled"], false);
    assert!(reply.body["github_login"].is_null());
    assert!(reply.body.get("password_hash").is_none());

    let cookie = set_cookie(&reply);
    assert!(cookie.starts_with("adus_session="));
    for attr in ["HttpOnly", "SameSite=Lax", "Path=/", "Max-Age=2592000"] {
        assert!(cookie.contains(attr), "{attr} missing in {cookie}");
    }
    assert!(!cookie.contains("Secure"), "http url must not set Secure");

    let me = send(&app, get("/api/v1/auth/me", Some(&cookie_pair(&reply)))).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["email"], "admin@example.com");
}

#[sqlx::test(migrations = "../../migrations")]
async fn cookie_is_secure_when_public_url_is_https(pool: PgPool) {
    let app = app_with_url(pool, "https://adus.example.com");
    let body = creds("admin@example.com", "correct horse");
    let reply = send(&app, post_json("/api/v1/auth/setup", &body, None)).await;
    assert!(set_cookie(&reply).contains("; Secure"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn setup_twice_is_409(pool: PgPool) {
    let app = app(pool);
    let body = creds("admin@example.com", "correct horse");
    let first = send(&app, post_json("/api/v1/auth/setup", &body, None)).await;
    assert_eq!(first.status, StatusCode::CREATED);

    let other = creds("other@example.com", "another password");
    let second = send(&app, post_json("/api/v1/auth/setup", &other, None)).await;
    assert_eq!(second.status, StatusCode::CONFLICT);
    assert_eq!(second.body["error"]["code"], "already_set_up");
}

#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_setup_creates_only_one_admin(pool: PgPool) {
    let app = app(pool.clone());
    let a = creds("a@example.com", "correct horse");
    let b = creds("b@example.com", "correct horse");
    let (ra, rb) = tokio::join!(
        send(&app, post_json("/api/v1/auth/setup", &a, None)),
        send(&app, post_json("/api/v1/auth/setup", &b, None)),
    );

    let mut statuses = [ra.status, rb.status];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::CREATED, StatusCode::CONFLICT]);
    assert_eq!(adus_db::users::count(&pool).await.unwrap(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn login_wrong_password_401_same_message_as_unknown_email(pool: PgPool) {
    let app = app(pool);
    let body = creds("admin@example.com", "correct horse");
    send(&app, post_json("/api/v1/auth/setup", &body, None)).await;

    let wrong_pw = creds("admin@example.com", "wrong password");
    let wrong_pw = send(&app, post_json("/api/v1/auth/login", &wrong_pw, None)).await;
    let no_user = creds("nobody@example.com", "wrong password");
    let no_user = send(&app, post_json("/api/v1/auth/login", &no_user, None)).await;

    assert_eq!(wrong_pw.status, StatusCode::UNAUTHORIZED);
    assert_eq!(no_user.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong_pw.body["error"]["code"], "invalid_credentials");
    assert_eq!(wrong_pw.body, no_user.body);
    assert!(wrong_pw.headers.get("set-cookie").is_none());
}

#[sqlx::test(migrations = "../../migrations")]
async fn login_succeeds_with_correct_password_case_insensitive_email(pool: PgPool) {
    let app = app(pool);
    let body = creds("admin@example.com", "correct horse");
    send(&app, post_json("/api/v1/auth/setup", &body, None)).await;

    let body = creds("ADMIN@Example.com", "correct horse");
    let reply = send(&app, post_json("/api/v1/auth/login", &body, None)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["role"], "admin");

    let me = send(&app, get("/api/v1/auth/me", Some(&cookie_pair(&reply)))).await;
    assert_eq!(me.status, StatusCode::OK);
}

#[sqlx::test(migrations = "../../migrations")]
async fn disabled_user_cannot_login(pool: PgPool) {
    let app = app(pool.clone());
    let body = creds("admin@example.com", "correct horse");
    let setup = send(&app, post_json("/api/v1/auth/setup", &body, None)).await;
    let id = setup.body["id"].as_str().unwrap().parse().unwrap();
    let patch = adus_db::users::UserPatch {
        disabled: Some(true),
        ..Default::default()
    };
    adus_db::users::update(&pool, id, patch).await.unwrap();

    let reply = send(&app, post_json("/api/v1/auth/login", &body, None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.body["error"]["code"], "invalid_credentials");
}

#[sqlx::test(migrations = "../../migrations")]
async fn me_without_cookie_401(pool: PgPool) {
    let app = app(pool);
    let reply = send(&app, get("/api/v1/auth/me", None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.body["error"]["code"], "unauthenticated");

    let reply = send(&app, get("/api/v1/auth/me", Some("adus_session=bogus"))).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations")]
async fn logout_invalidates_session(pool: PgPool) {
    let app = app(pool.clone());
    let body = creds("admin@example.com", "correct horse");
    let setup = send(&app, post_json("/api/v1/auth/setup", &body, None)).await;
    let cookie = cookie_pair(&setup);

    let reply = send(&app, post_empty("/api/v1/auth/logout", Some(&cookie))).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let cleared = set_cookie(&reply);
    assert!(cleared.starts_with("adus_session=;"));
    assert!(cleared.contains("Max-Age=0"));

    let me = send(&app, get("/api/v1/auth/me", Some(&cookie))).await;
    assert_eq!(me.status, StatusCode::UNAUTHORIZED);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn session_token_is_stored_hashed(pool: PgPool) {
    let app = app(pool.clone());
    let body = creds("admin@example.com", "correct horse");
    let setup = send(&app, post_json("/api/v1/auth/setup", &body, None)).await;
    let token = cookie_pair(&setup)["adus_session=".len()..].to_string();

    let stored: String = sqlx::query_scalar("SELECT token_hash FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_ne!(stored, token);
    assert_eq!(stored.len(), 64);
    assert!(stored.chars().all(|c| c.is_ascii_hexdigit()));
}

#[sqlx::test(migrations = "../../migrations")]
async fn password_shorter_than_8_is_422(pool: PgPool) {
    let app = app(pool);
    let body = creds("admin@example.com", "short");
    let reply = send(&app, post_json("/api/v1/auth/setup", &body, None)).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "validation_error");

    let status = send(&app, get("/api/v1/auth/status", None)).await;
    assert_eq!(status.body, json!({ "needs_setup": true }));
}

#[sqlx::test(migrations = "../../migrations")]
async fn email_without_at_is_422(pool: PgPool) {
    let app = app(pool);
    let body = creds("  not-an-email ", "correct horse");
    let reply = send(&app, post_json("/api/v1/auth/setup", &body, None)).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "validation_error");
}
