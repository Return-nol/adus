//! Helper bersama untuk test integrasi. Tiap file test hanya memakai sebagian, jadi `dead_code` diizinkan.
#![allow(dead_code)]

use std::{collections::HashMap, sync::Arc};

use adus_core::{config::Config, crypto::Cipher};
use adus_server::{AppState, router};
use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

/// base64 dari 32 byte nol (sama dengan `.cargo/config.toml`).
const TEST_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

pub fn test_config(public_url: &str) -> Config {
    let env: HashMap<String, String> = [
        ("DATABASE_URL", "postgres://x"),
        ("REDIS_URL", "redis://x"),
        ("ADUS_SECRET_KEY", TEST_KEY),
        ("ADUS_PUBLIC_URL", public_url),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    Config::from_map(&env).unwrap()
}

pub fn app_with_url(pool: PgPool, public_url: &str) -> Router {
    router(AppState {
        db: pool,
        cipher: Arc::new(Cipher::from_base64_key(TEST_KEY).unwrap()),
        config: Arc::new(test_config(public_url)),
    })
}

pub fn app(pool: PgPool) -> Router {
    app_with_url(pool, "http://localhost:8080")
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// `Value::Null` jika body kosong.
    pub body: Value,
}

/// Kirim request ke router (router di-clone sehingga bisa dipakai berulang).
pub async fn send(app: &Router, req: Request<Body>) -> Reply {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Reply {
        status,
        headers,
        body,
    }
}

pub fn get(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut req = Request::get(uri);
    if let Some(cookie) = cookie {
        req = req.header(header::COOKIE, cookie);
    }
    req.body(Body::empty()).unwrap()
}

pub fn post_json(uri: &str, body: &Value, cookie: Option<&str>) -> Request<Body> {
    let mut req = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        req = req.header(header::COOKIE, cookie);
    }
    req.body(Body::from(body.to_string())).unwrap()
}

/// Request JSON dengan method bebas (PUT, PATCH, ...).
pub fn json_req(method: &str, uri: &str, body: &Value, cookie: Option<&str>) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        req = req.header(header::COOKIE, cookie);
    }
    req.body(Body::from(body.to_string())).unwrap()
}

pub fn post_empty(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut req = Request::post(uri);
    if let Some(cookie) = cookie {
        req = req.header(header::COOKIE, cookie);
    }
    req.body(Body::empty()).unwrap()
}

/// Seluruh nilai `Set-Cookie` (atribut ikut).
pub fn set_cookie(reply: &Reply) -> String {
    reply
        .headers
        .get(header::SET_COOKIE)
        .expect("Set-Cookie header")
        .to_str()
        .unwrap()
        .to_string()
}

/// Bagian `adus_session=<token>` saja, siap dipakai sebagai header `Cookie`.
pub fn cookie_pair(reply: &Reply) -> String {
    set_cookie(reply).split(';').next().unwrap().to_string()
}
