//! Helper bersama untuk test integrasi. Tiap file test hanya memakai sebagian, jadi `dead_code` diizinkan.
#![allow(dead_code)]

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use adus_core::{config::Config, crypto::Cipher};
use adus_git::{
    ConnectionKind, FileDiff, GitError, GitProvider, GitProviderFactory, InlineComment, PatOwner,
    PullRequest, RemoteRepo, RepoRef,
};
use adus_server::{AppState, router};
use async_trait::async_trait;
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

pub fn test_cipher() -> Cipher {
    Cipher::from_base64_key(TEST_KEY).unwrap()
}

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
    app_with_git(pool, public_url, Arc::new(FakeGitFactory::default()))
}

pub fn app_with_git(pool: PgPool, public_url: &str, git: Arc<FakeGitFactory>) -> Router {
    router(AppState {
        db: pool,
        cipher: Arc::new(test_cipher()),
        config: Arc::new(test_config(public_url)),
        git,
        http: reqwest::Client::new(),
    })
}

pub fn app(pool: PgPool) -> Router {
    app_with_url(pool, "http://localhost:8080")
}

/// Router dengan `FakeGitFactory` yang bisa diatur dan diperiksa test.
pub fn app_and_git(pool: PgPool) -> (Router, Arc<FakeGitFactory>) {
    let git = Arc::new(FakeGitFactory::default());
    (
        app_with_git(pool, "http://localhost:8080", git.clone()),
        git,
    )
}

/// Satu panggilan `ensure_webhooks` yang tercatat.
#[derive(Debug, Clone)]
pub struct EnsureCall {
    pub token: String,
    pub external_id: String,
    pub url: String,
    pub secret: String,
    pub existing: Vec<String>,
}

/// Panggilan `delete_webhooks` yang tercatat: (token, external_id repo, id webhook).
pub type DeleteCall = (String, String, Vec<String>);

/// Keadaan bersama `FakeGitFactory` dan semua `FakeGit` yang dibuatnya.
pub struct FakeGitState {
    /// Token di sini ditolak semua method dengan `Unauthorized`.
    pub bad_tokens: HashSet<String>,
    /// Jika diisi, `verify` gagal dengan error ini (setelah cek `bad_tokens`).
    pub verify_error: Option<GitError>,
    pub owner: PatOwner,
    pub repos: Vec<RemoteRepo>,
    /// `ensure_webhooks` gagal untuk repo dengan external_id ini.
    pub ensure_errors: HashMap<String, GitError>,
    pub delete_error: Option<GitError>,
    pub ensure_calls: Vec<EnsureCall>,
    pub delete_calls: Vec<DeleteCall>,
}

impl Default for FakeGitState {
    fn default() -> Self {
        FakeGitState {
            bad_tokens: HashSet::new(),
            verify_error: None,
            owner: PatOwner {
                id: "1".into(),
                login: "octocat".into(),
                email: None,
            },
            repos: Vec::new(),
            ensure_errors: HashMap::new(),
            delete_error: None,
            ensure_calls: Vec::new(),
            delete_calls: Vec::new(),
        }
    }
}

#[derive(Default)]
pub struct FakeGitFactory {
    pub state: Arc<Mutex<FakeGitState>>,
}

impl FakeGitFactory {
    /// Ubah keadaan fake, mis. `git.with(|s| s.bad_tokens.insert("x".into()))`.
    pub fn with<R>(&self, f: impl FnOnce(&mut FakeGitState) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    pub fn ensure_calls(&self) -> Vec<EnsureCall> {
        self.with(|s| s.ensure_calls.clone())
    }

    pub fn delete_calls(&self) -> Vec<DeleteCall> {
        self.with(|s| s.delete_calls.clone())
    }
}

impl GitProviderFactory for FakeGitFactory {
    fn create(&self, _kind: ConnectionKind, _org: &str, token: &str) -> Arc<dyn GitProvider> {
        Arc::new(FakeGit {
            token: token.to_string(),
            state: self.state.clone(),
        })
    }
}

/// Provider palsu untuk satu token. Method yang belum dibutuhkan task ini mengembalikan error.
pub struct FakeGit {
    token: String,
    state: Arc<Mutex<FakeGitState>>,
}

impl FakeGit {
    fn check_token(&self, state: &FakeGitState) -> Result<(), GitError> {
        if state.bad_tokens.contains(&self.token) {
            return Err(GitError::Unauthorized);
        }
        Ok(())
    }
}

#[async_trait]
impl GitProvider for FakeGit {
    async fn verify(&self) -> Result<PatOwner, GitError> {
        let state = self.state.lock().unwrap();
        self.check_token(&state)?;
        match &state.verify_error {
            Some(err) => Err(err.clone()),
            None => Ok(state.owner.clone()),
        }
    }

    async fn list_repos(&self) -> Result<Vec<RemoteRepo>, GitError> {
        let state = self.state.lock().unwrap();
        self.check_token(&state)?;
        Ok(state.repos.clone())
    }

    /// Mengembalikan `existing`, atau satu id baru `hook-<external_id>` jika kosong.
    async fn ensure_webhooks(
        &self,
        repo: &RepoRef,
        url: &str,
        secret: &str,
        existing: &[String],
    ) -> Result<Vec<String>, GitError> {
        let mut state = self.state.lock().unwrap();
        self.check_token(&state)?;
        state.ensure_calls.push(EnsureCall {
            token: self.token.clone(),
            external_id: repo.external_id.clone(),
            url: url.to_string(),
            secret: secret.to_string(),
            existing: existing.to_vec(),
        });
        if let Some(err) = state.ensure_errors.get(&repo.external_id) {
            return Err(err.clone());
        }
        if existing.is_empty() {
            return Ok(vec![format!("hook-{}", repo.external_id)]);
        }
        Ok(existing.to_vec())
    }

    async fn delete_webhooks(&self, repo: &RepoRef, ids: &[String]) -> Result<(), GitError> {
        let mut state = self.state.lock().unwrap();
        state
            .delete_calls
            .push((self.token.clone(), repo.external_id.clone(), ids.to_vec()));
        match &state.delete_error {
            Some(err) => Err(err.clone()),
            None => Ok(()),
        }
    }

    async fn get_pr(&self, _repo: &RepoRef, _number: i64) -> Result<PullRequest, GitError> {
        Err(GitError::Http("FakeGit: get_pr not implemented".into()))
    }

    async fn pr_diff(&self, _repo: &RepoRef, _pr: &PullRequest) -> Result<Vec<FileDiff>, GitError> {
        Err(GitError::Http("FakeGit: pr_diff not implemented".into()))
    }

    async fn diff_between(
        &self,
        _repo: &RepoRef,
        _base_sha: &str,
        _head_sha: &str,
    ) -> Result<Vec<FileDiff>, GitError> {
        Err(GitError::Http(
            "FakeGit: diff_between not implemented".into(),
        ))
    }

    async fn post_review(
        &self,
        _repo: &RepoRef,
        _pr: &PullRequest,
        _comments: &[InlineComment],
        _summary: &str,
    ) -> Result<usize, GitError> {
        Err(GitError::Http(
            "FakeGit: post_review not implemented".into(),
        ))
    }

    async fn post_comment(
        &self,
        _repo: &RepoRef,
        _pr_number: i64,
        _body: &str,
    ) -> Result<(), GitError> {
        Err(GitError::Http(
            "FakeGit: post_comment not implemented".into(),
        ))
    }
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
