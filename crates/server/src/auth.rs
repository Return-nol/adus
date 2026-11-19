use std::sync::OnceLock;

use adus_db::{
    sessions,
    users::{self, Role, User},
};
use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{
        SaltString,
        rand_core::{OsRng, RngCore},
    },
};
use axum::{
    Json, Router,
    extract::{FromRequestParts, State},
    http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    error::{ApiError, ApiJson},
    state::AppState,
};

const COOKIE_NAME: &str = "adus_session";
const SESSION_DAYS: i64 = 30;
const MIN_PASSWORD_LEN: usize = 8;

pub fn routes() -> Router<AppState> {
    // Hitung hash palsu sekarang (saat start), bukan di request login pertama.
    dummy_hash();
    Router::new()
        .route("/auth/status", get(status))
        .route("/auth/setup", post(setup))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
}

/// Bentuk user di JSON (tanpa hash password).
#[derive(Debug, Serialize)]
pub struct UserJson {
    pub id: Uuid,
    pub email: String,
    pub role: Role,
    pub disabled: bool,
    pub github_login: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl From<User> for UserJson {
    fn from(u: User) -> UserJson {
        UserJson {
            id: u.id,
            email: u.email,
            role: u.role,
            disabled: u.disabled,
            github_login: u.github_login,
            created_at: u.created_at,
        }
    }
}

#[derive(Deserialize)]
struct Credentials {
    email: String,
    password: String,
}

async fn status(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let needs_setup = users::count(&state.db).await? == 0;
    Ok(Json(serde_json::json!({ "needs_setup": needs_setup })))
}

async fn setup(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<Credentials>,
) -> Result<Response, ApiError> {
    let email = body.email.trim();
    validate_email(email)?;
    validate_password(&body.password)?;

    // Cek murah sebelum hashing; transaksi di bawah tetap penjaga yang menentukan.
    if users::count(&state.db).await? > 0 {
        return Err(already_set_up());
    }

    let hash = hash_password(body.password).await?;
    let Some(user) = users::insert_first_admin(&state.db, email, &hash).await? else {
        return Err(already_set_up());
    };

    let cookie = start_session(&state, &user).await?;
    let reply = (
        StatusCode::CREATED,
        cookie_headers(cookie),
        Json(UserJson::from(user)),
    );
    Ok(reply.into_response())
}

pub(crate) fn validate_email(email: &str) -> Result<(), ApiError> {
    if email.contains('@') {
        Ok(())
    } else {
        Err(ApiError::validation("email must contain '@'"))
    }
}

pub(crate) fn validate_password(password: &str) -> Result<(), ApiError> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(ApiError::validation(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    Ok(())
}

fn already_set_up() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "already_set_up",
        "Adus is already set up",
    )
}

async fn login(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<Credentials>,
) -> Result<Response, ApiError> {
    let found = users::by_email(&state.db, body.email.trim()).await?;
    // Email tidak dikenal tetap memverifikasi hash palsu supaya waktu respons mirip.
    let hash = found.as_ref().map(|user| user.password_hash.clone());
    let password_ok = verify_password(body.password, hash).await?;

    match found {
        Some(user) if password_ok && !user.disabled => {
            let cookie = start_session(&state, &user).await?;
            Ok((cookie_headers(cookie), Json(UserJson::from(user))).into_response())
        }
        _ => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "invalid email or password",
        )),
    }
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    if let Some(token) = session_token(&headers) {
        sessions::delete(&state.db, &hash_token(&token)).await?;
    }
    let clear = format!(
        "{COOKIE_NAME}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0{}",
        secure_attr(&state)
    );
    Ok((StatusCode::NO_CONTENT, cookie_headers(clear)).into_response())
}

async fn me(CurrentUser(user): CurrentUser) -> Json<UserJson> {
    Json(user.into())
}

/// User dari cookie sesi yang valid; 401 `unauthenticated` jika tidak ada.
pub struct CurrentUser(pub User);

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<CurrentUser, ApiError> {
        let token = session_token(&parts.headers).ok_or_else(ApiError::unauthenticated)?;
        let user = sessions::user_for(&state.db, &hash_token(&token)).await?;
        user.map(CurrentUser).ok_or_else(ApiError::unauthenticated)
    }
}

/// Seperti `CurrentUser`, tetapi 403 `forbidden` jika bukan admin.
pub struct AdminUser(pub User);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<AdminUser, ApiError> {
        let CurrentUser(user) = CurrentUser::from_request_parts(parts, state).await?;
        if user.role != Role::Admin {
            return Err(ApiError::forbidden());
        }
        Ok(AdminUser(user))
    }
}

/// Buat sesi baru di DB dan kembalikan nilai header `Set-Cookie`.
async fn start_session(state: &AppState, user: &User) -> Result<String, ApiError> {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = URL_SAFE_NO_PAD.encode(bytes);

    let expires_at = Utc::now() + Duration::days(SESSION_DAYS);
    sessions::create(&state.db, user.id, &hash_token(&token), expires_at).await?;

    Ok(format!(
        "{COOKIE_NAME}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}{}",
        SESSION_DAYS * 24 * 60 * 60,
        secure_attr(state)
    ))
}

fn secure_attr(state: &AppState) -> &'static str {
    if state.config.public_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    }
}

fn cookie_headers(cookie: String) -> HeaderMap {
    let mut headers = HeaderMap::new();
    // Cookie hanya berisi karakter ASCII yang kita susun sendiri, jadi selalu valid.
    headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("cookie is valid ASCII"),
    );
    headers
}

/// Token mentah dari header `Cookie`, jika ada.
pub(crate) fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE_NAME)
        .map(|(_, value)| value.to_string())
}

/// Yang disimpan di DB: SHA-256 heksadesimal dari token.
pub(crate) fn hash_token(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn hash_password_sync(password: &str) -> Result<String, argon2::password_hash::Error> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

/// Hash argon2 dihitung di thread blocking supaya runtime async tidak tersendat.
pub(crate) async fn hash_password(password: String) -> Result<String, ApiError> {
    match tokio::task::spawn_blocking(move || hash_password_sync(&password)).await {
        Ok(Ok(hash)) => Ok(hash),
        Ok(Err(err)) => {
            tracing::error!("password hashing failed: {err}");
            Err(ApiError::internal())
        }
        Err(err) => {
            tracing::error!("password hashing task failed: {err}");
            Err(ApiError::internal())
        }
    }
}

/// `false` jika password salah atau hash tidak bisa di-parse.
/// `hash = None` (user tidak ada) memverifikasi hash palsu; semuanya di thread blocking.
pub(crate) async fn verify_password(
    password: String,
    hash: Option<String>,
) -> Result<bool, ApiError> {
    let verified = tokio::task::spawn_blocking(move || {
        let hash = hash.unwrap_or_else(|| dummy_hash().to_string());
        match PasswordHash::new(&hash) {
            Ok(parsed) => Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok(),
            Err(_) => false,
        }
    })
    .await;
    verified.map_err(|err| {
        tracing::error!("password verification task failed: {err}");
        ApiError::internal()
    })
}

/// Hash argon2 sungguhan dari password tetap, dipakai untuk email yang tidak dikenal.
fn dummy_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| hash_password_sync("adus-dummy-password").expect("dummy hash"))
}
