use adus_db::{
    sessions,
    users::{self, GuardedUpdate, Role, UserPatch},
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, patch, post, put},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    auth::{
        AdminUser, CurrentUser, UserJson, hash_password, hash_token, session_token, validate_email,
        validate_password, verify_password,
    },
    error::{ApiError, ApiJson, ApiPath},
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/users", get(list).post(create))
        .route("/users/{id}", patch(update))
        .route("/users/{id}/password", post(reset_password))
        .route("/me", put(update_me))
        .route("/me/password", put(change_password))
}

#[derive(Deserialize)]
struct CreateUser {
    email: String,
    password: String,
    role: Role,
    github_login: Option<String>,
}

#[derive(Deserialize)]
struct UpdateUser {
    role: Option<Role>,
    disabled: Option<bool>,
    github_login: Option<String>,
}

#[derive(Deserialize)]
struct SetPassword {
    password: String,
}

#[derive(Deserialize)]
struct UpdateMe {
    github_login: String,
}

#[derive(Deserialize)]
struct ChangePassword {
    current_password: String,
    new_password: String,
}

async fn list(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<UserJson>>, ApiError> {
    let all = users::list(&state.db).await?;
    Ok(Json(all.into_iter().map(UserJson::from).collect()))
}

async fn create(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateUser>,
) -> Result<(StatusCode, Json<UserJson>), ApiError> {
    let email = body.email.trim();
    validate_email(email)?;
    validate_password(&body.password)?;
    // Login kosong sama dengan tidak diisi.
    let github_login = body.github_login.and_then(clean_github_login);

    let hash = hash_password(body.password).await?;
    let user = users::insert(&state.db, email, &hash, body.role, github_login.as_deref())
        .await
        .map_err(unique_violation)?;
    Ok((StatusCode::CREATED, Json(user.into())))
}

async fn update(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<UpdateUser>,
) -> Result<Json<UserJson>, ApiError> {
    let disabling = body.disabled == Some(true);
    let patch = UserPatch {
        role: body.role,
        disabled: body.disabled,
        github_login: body.github_login.map(clean_github_login),
    };
    let user = match users::update_guarded(&state.db, id, patch)
        .await
        .map_err(unique_violation)?
    {
        GuardedUpdate::Updated(user) => user,
        GuardedUpdate::NotFound => return Err(ApiError::not_found()),
        GuardedUpdate::LastAdmin => {
            return Err(ApiError::conflict(
                "last_admin",
                "at least one active admin must remain",
            ));
        }
    };
    if disabling {
        sessions::delete_for_user(&state.db, id, None).await?;
    }
    Ok(Json(user.into()))
}

async fn reset_password(
    AdminUser(admin): AdminUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<SetPassword>,
) -> Result<StatusCode, ApiError> {
    validate_password(&body.password)?;
    if users::by_id(&state.db, id).await?.is_none() {
        return Err(ApiError::not_found());
    }
    let hash = hash_password(body.password).await?;
    users::set_password(&state.db, id, &hash).await?;
    // Mengganti password sendiri lewat jalur admin: sesi yang sedang dipakai tetap hidup.
    let current = (admin.id == id)
        .then(|| session_token(&headers).map(|token| hash_token(&token)))
        .flatten();
    sessions::delete_for_user(&state.db, id, current.as_deref()).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn update_me(
    CurrentUser(user): CurrentUser,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<UpdateMe>,
) -> Result<Json<UserJson>, ApiError> {
    let patch = UserPatch {
        github_login: Some(clean_github_login(body.github_login)),
        ..Default::default()
    };
    let user = users::update(&state.db, user.id, patch)
        .await
        .map_err(unique_violation)?;
    Ok(Json(user.into()))
}

async fn change_password(
    CurrentUser(user): CurrentUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<ChangePassword>,
) -> Result<StatusCode, ApiError> {
    validate_password(&body.new_password)?;
    if !verify_password(body.current_password, Some(user.password_hash.clone())).await? {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_current_password",
            "current password is incorrect",
        ));
    }
    let hash = hash_password(body.new_password).await?;
    users::set_password(&state.db, user.id, &hash).await?;

    // Sesi lain berhenti berlaku; sesi yang sedang dipakai tetap hidup.
    let current = session_token(&headers).map(|token| hash_token(&token));
    sessions::delete_for_user(&state.db, user.id, current.as_deref()).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Spasi dipangkas; string kosong berarti hapus (`None`, di patch dibungkus `Some`).
fn clean_github_login(login: String) -> Option<String> {
    let login = login.trim();
    (!login.is_empty()).then(|| login.to_string())
}

/// Pelanggaran unique pada email / github_login menjadi 409; error lain tetap 500.
fn unique_violation(err: sqlx::Error) -> ApiError {
    let constraint = err
        .as_database_error()
        .filter(|db_err| db_err.is_unique_violation())
        .and_then(|db_err| db_err.constraint());
    match constraint {
        Some(name) if name.contains("github_login") => {
            ApiError::conflict("github_login_taken", "github login is already used")
        }
        Some(name) if name.contains("email") => {
            ApiError::conflict("email_taken", "email is already used")
        }
        _ => ApiError::from(err),
    }
}
