mod common;

use axum::{Router, http::StatusCode};
use common::{app, cookie_pair, get, json_req, post_json, send};
use serde_json::{Value, json};
use sqlx::PgPool;

const PASSWORD: &str = "correct horse";

/// Admin pertama lewat setup; mengembalikan cookie sesinya.
async fn setup_admin(app: &Router) -> String {
    let body = json!({ "email": "admin@example.com", "password": PASSWORD });
    let reply = send(app, post_json("/api/v1/auth/setup", &body, None)).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    cookie_pair(&reply)
}

async fn login(app: &Router, email: &str, password: &str) -> common::Reply {
    let body = json!({ "email": email, "password": password });
    send(app, post_json("/api/v1/auth/login", &body, None)).await
}

/// Admin membuat user lalu login sebagai user itu; mengembalikan (id, cookie).
async fn create_and_login(app: &Router, admin: &str, email: &str, role: &str) -> (String, String) {
    let body = json!({ "email": email, "password": PASSWORD, "role": role });
    let created = send(app, post_json("/api/v1/users", &body, Some(admin))).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.body["id"].as_str().unwrap().to_string();
    let reply = login(app, email, PASSWORD).await;
    assert_eq!(reply.status, StatusCode::OK);
    (id, cookie_pair(&reply))
}

async fn admin_id(app: &Router, cookie: &str) -> String {
    let me = send(app, get("/api/v1/auth/me", Some(cookie))).await;
    me.body["id"].as_str().unwrap().to_string()
}

async fn patch_user(app: &Router, admin: &str, id: &str, body: &Value) -> common::Reply {
    let uri = format!("/api/v1/users/{id}");
    send(app, json_req("PATCH", &uri, body, Some(admin))).await
}

async fn status_of_me(app: &Router, cookie: &str) -> StatusCode {
    send(app, get("/api/v1/auth/me", Some(cookie))).await.status
}

#[sqlx::test(migrations = "../../migrations")]
async fn user_role_cannot_list_users_403(pool: PgPool) {
    let app = app(pool);
    let admin = setup_admin(&app).await;
    let (_, user) = create_and_login(&app, &admin, "u@example.com", "user").await;

    let reply = send(&app, get("/api/v1/users", Some(&user))).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.body["error"]["code"], "forbidden");

    let reply = send(&app, get("/api/v1/users", None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    let reply = send(&app, get("/api/v1/users", Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body.as_array().unwrap().len(), 2);
}

#[sqlx::test(migrations = "../../migrations")]
async fn admin_creates_user(pool: PgPool) {
    let app = app(pool);
    let admin = setup_admin(&app).await;
    let body = json!({
        "email": " new@example.com ", "password": PASSWORD,
        "role": "user", "github_login": " octo "
    });
    let reply = send(&app, post_json("/api/v1/users", &body, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.body["email"], "new@example.com");
    assert_eq!(reply.body["role"], "user");
    assert_eq!(reply.body["github_login"], "octo");
    assert!(reply.body.get("password_hash").is_none());

    // User baru bisa login; email yang sama (beda huruf) ditolak.
    let reply = login(&app, "new@example.com", PASSWORD).await;
    assert_eq!(reply.status, StatusCode::OK);
    let again = json!({ "email": "NEW@example.com", "password": PASSWORD, "role": "user" });
    let reply = send(&app, post_json("/api/v1/users", &again, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["error"]["code"], "email_taken");
}

#[sqlx::test(migrations = "../../migrations")]
async fn create_user_validates_input(pool: PgPool) {
    let app = app(pool);
    let admin = setup_admin(&app).await;
    for body in [
        json!({ "email": "nope", "password": PASSWORD, "role": "user" }),
        json!({ "email": "a@example.com", "password": "short", "role": "user" }),
        json!({ "email": "a@example.com", "password": PASSWORD, "role": "root" }),
    ] {
        let reply = send(&app, post_json("/api/v1/users", &body, Some(&admin))).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(reply.body["error"]["code"], "validation_error");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn cannot_demote_last_active_admin_409(pool: PgPool) {
    let app = app(pool);
    let admin = setup_admin(&app).await;
    let id = admin_id(&app, &admin).await;

    let demote = json!({ "role": "user" });
    let reply = patch_user(&app, &admin, &id, &demote).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["error"]["code"], "last_admin");

    // Dengan admin kedua, demosi diizinkan.
    create_and_login(&app, &admin, "b@example.com", "admin").await;
    let reply = patch_user(&app, &admin, &id, &demote).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["role"], "user");
}

#[sqlx::test(migrations = "../../migrations")]
async fn cannot_disable_last_active_admin_409(pool: PgPool) {
    let app = app(pool);
    let admin = setup_admin(&app).await;
    let id = admin_id(&app, &admin).await;

    let disable = json!({ "disabled": true });
    let reply = patch_user(&app, &admin, &id, &disable).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["error"]["code"], "last_admin");

    // Admin kedua yang sudah dinonaktifkan tidak dihitung sebagai admin aktif.
    let (b_id, _) = create_and_login(&app, &admin, "b@example.com", "admin").await;
    let reply = patch_user(&app, &admin, &b_id, &disable).await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = patch_user(&app, &admin, &id, &disable).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = "../../migrations")]
async fn disabling_user_ends_their_sessions(pool: PgPool) {
    let app = app(pool);
    let admin = setup_admin(&app).await;
    let (id, user) = create_and_login(&app, &admin, "u@example.com", "user").await;

    let reply = patch_user(&app, &admin, &id, &json!({ "disabled": true })).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["disabled"], true);
    assert_eq!(status_of_me(&app, &user).await, StatusCode::UNAUTHORIZED);

    // Diaktifkan kembali: sesi lama tetap hilang, tetapi login baru berhasil.
    patch_user(&app, &admin, &id, &json!({ "disabled": false })).await;
    assert_eq!(status_of_me(&app, &user).await, StatusCode::UNAUTHORIZED);
    let reply = login(&app, "u@example.com", PASSWORD).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[sqlx::test(migrations = "../../migrations")]
async fn patch_unknown_or_malformed_id_is_404(pool: PgPool) {
    let app = app(pool);
    let admin = setup_admin(&app).await;
    let patch = json!({ "disabled": true });
    for id in ["00000000-0000-0000-0000-000000000000", "not-a-uuid"] {
        let reply = patch_user(&app, &admin, id, &patch).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{id}");
        assert_eq!(reply.body["error"]["code"], "not_found");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn github_login_unique_409(pool: PgPool) {
    let app = app(pool);
    let admin = setup_admin(&app).await;
    let (b_id, b) = create_and_login(&app, &admin, "b@example.com", "user").await;

    let set = json!({ "github_login": "Octo" });
    let reply = send(&app, json_req("PUT", "/api/v1/me", &set, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["github_login"], "Octo");

    // Self-service (case-insensitive) dan lewat admin PATCH sama-sama 409.
    let same = json!({ "github_login": "octo" });
    let reply = send(&app, json_req("PUT", "/api/v1/me", &same, Some(&b))).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["error"]["code"], "github_login_taken");
    let reply = patch_user(&app, &admin, &b_id, &same).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["error"]["code"], "github_login_taken");

    // Membuat user dengan login yang dipakai juga 409, dan user tidak ikut terbuat.
    let body = json!({
        "email": "c@example.com", "password": PASSWORD,
        "role": "user", "github_login": "OCTO"
    });
    let reply = send(&app, post_json("/api/v1/users", &body, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    let list = send(&app, get("/api/v1/users", Some(&admin))).await;
    assert_eq!(list.body.as_array().unwrap().len(), 2);

    // String kosong menghapus login; setelah itu boleh dipakai user lain.
    let clear = json!({ "github_login": "  " });
    let reply = send(&app, json_req("PUT", "/api/v1/me", &clear, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["github_login"], Value::Null);
    let reply = send(&app, json_req("PUT", "/api/v1/me", &same, Some(&b))).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[sqlx::test(migrations = "../../migrations")]
async fn change_own_password_requires_current(pool: PgPool) {
    let app = app(pool);
    let admin = setup_admin(&app).await;
    let (_, user) = create_and_login(&app, &admin, "u@example.com", "user").await;
    // Sesi kedua milik user yang sama.
    let other = cookie_pair(&login(&app, "u@example.com", PASSWORD).await);
    let put = |body: Value| json_req("PUT", "/api/v1/me/password", &body, Some(&user));

    let wrong = json!({ "current_password": "wrong password", "new_password": "brand new pass" });
    let reply = send(&app, put(wrong)).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.body["error"]["code"], "invalid_current_password");
    let reply = login(&app, "u@example.com", PASSWORD).await;
    assert_eq!(reply.status, StatusCode::OK);

    let short = json!({ "current_password": PASSWORD, "new_password": "short" });
    let reply = send(&app, put(short)).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);

    let ok = json!({ "current_password": PASSWORD, "new_password": "brand new pass" });
    let reply = send(&app, put(ok)).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);

    let reply = login(&app, "u@example.com", PASSWORD).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let reply = login(&app, "u@example.com", "brand new pass").await;
    assert_eq!(reply.status, StatusCode::OK);
    // Sesi saat ini tetap hidup, sesi lain dicabut.
    assert_eq!(status_of_me(&app, &user).await, StatusCode::OK);
    assert_eq!(status_of_me(&app, &other).await, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations")]
async fn admin_resets_password_and_ends_sessions(pool: PgPool) {
    let app = app(pool);
    let admin = setup_admin(&app).await;
    let (id, user) = create_and_login(&app, &admin, "u@example.com", "user").await;
    let uri = format!("/api/v1/users/{id}/password");

    let short = json!({ "password": "short" });
    let reply = send(&app, post_json(&uri, &short, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);

    // Non-admin tidak boleh; admin boleh.
    let body = json!({ "password": "reset by admin" });
    let reply = send(&app, post_json(&uri, &body, Some(&user))).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = send(&app, post_json(&uri, &body, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);

    assert_eq!(status_of_me(&app, &user).await, StatusCode::UNAUTHORIZED);
    let reply = login(&app, "u@example.com", PASSWORD).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let reply = login(&app, "u@example.com", "reset by admin").await;
    assert_eq!(reply.status, StatusCode::OK);

    let missing = "/api/v1/users/00000000-0000-0000-0000-000000000000/password";
    let reply = send(&app, post_json(missing, &body, Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}
