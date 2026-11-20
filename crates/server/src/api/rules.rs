use adus_core::types::Severity;
use adus_db::{
    repositories,
    rules::{self, KnowledgeScope, NewRule, Rule, RulePatch},
};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, patch},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::repositories::present_or_null;
use crate::{
    auth::CurrentUser,
    error::{ApiError, ApiJson, ApiPath, ApiQuery},
    state::AppState,
};

const TITLE_MAX: usize = 200;
const BODY_MAX: usize = 10_000;
const GLOB_MAX: usize = 500;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/rules", get(list).post(create))
        .route("/rules/{id}", patch(update).delete(remove))
}

/// Query untuk `GET /rules` dan `GET /memories`.
#[derive(Deserialize)]
pub(super) struct ListQuery {
    pub repository_id: Option<Uuid>,
    /// `true` = semua rule/memory (global dan semua repo), mengabaikan `repository_id`.
    #[serde(default)]
    pub all: bool,
}

#[derive(Deserialize)]
struct CreateRule {
    scope: KnowledgeScope,
    repository_id: Option<Uuid>,
    title: String,
    body: String,
    path_glob: Option<String>,
    severity: Option<Severity>,
}

#[derive(Deserialize)]
struct UpdateRule {
    title: Option<String>,
    body: Option<String>,
    /// Ada dan `null` (atau `""`) = hapus glob; tidak ada = biarkan.
    #[serde(default, deserialize_with = "present_or_null")]
    path_glob: Option<Option<String>>,
    severity: Option<Severity>,
    enabled: Option<bool>,
}

#[derive(Serialize)]
struct RuleJson {
    id: Uuid,
    scope: KnowledgeScope,
    repository_id: Option<Uuid>,
    title: String,
    body: String,
    path_glob: Option<String>,
    severity: Severity,
    enabled: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<Rule> for RuleJson {
    fn from(r: Rule) -> RuleJson {
        RuleJson {
            id: r.id,
            scope: r.scope,
            repository_id: r.repository_id,
            title: r.title,
            body: r.body,
            path_glob: r.path_glob,
            severity: r.severity,
            enabled: r.enabled,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// Teks yang dipangkas dan panjangnya (dalam karakter) harus 1..=`max`.
pub(super) fn trimmed(field: &str, value: &str, max: usize) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > max {
        return Err(ApiError::validation(format!(
            "{field} must be between 1 and {max} characters"
        )));
    }
    Ok(value.to_string())
}

/// Glob kosong = tidak ada glob. Glob yang tidak valid ditolak dengan 422 `invalid_glob`.
fn clean_glob(glob: Option<&str>) -> Result<Option<String>, ApiError> {
    let Some(glob) = glob.map(str::trim).filter(|g| !g.is_empty()) else {
        return Ok(None);
    };
    if glob.chars().count() > GLOB_MAX || !rules::validate_glob(glob) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_glob",
            "path_glob is not a valid glob pattern",
        ));
    }
    Ok(Some(glob.to_string()))
}

/// Scope harus cocok dengan `repository_id`, dan repo-nya harus aktif (404 jika tidak).
pub(super) async fn check_scope(
    state: &AppState,
    scope: KnowledgeScope,
    repository_id: Option<Uuid>,
) -> Result<(), ApiError> {
    match (scope, repository_id) {
        (KnowledgeScope::Global, None) => Ok(()),
        (KnowledgeScope::Global, Some(_)) => Err(ApiError::validation(
            "repository_id must be empty when scope is global",
        )),
        (KnowledgeScope::Repo, None) => Err(ApiError::validation(
            "repository_id is required when scope is repo",
        )),
        (KnowledgeScope::Repo, Some(id)) => repositories::get(&state.db, id)
            .await?
            .map(|_| ())
            .ok_or_else(ApiError::not_found),
    }
}

/// Repo dari `?repository_id=` harus aktif; dipakai oleh daftar rule dan memory.
pub(super) async fn check_repo(
    state: &AppState,
    repository_id: Option<Uuid>,
) -> Result<(), ApiError> {
    if let Some(id) = repository_id
        && repositories::get(&state.db, id).await?.is_none()
    {
        return Err(ApiError::not_found());
    }
    Ok(())
}

async fn list(
    _user: CurrentUser,
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<RuleJson>>, ApiError> {
    let found = if query.all {
        rules::list_all(&state.db).await?
    } else {
        check_repo(&state, query.repository_id).await?;
        rules::list(&state.db, query.repository_id).await?
    };
    Ok(Json(found.into_iter().map(RuleJson::from).collect()))
}

async fn create(
    _user: CurrentUser,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateRule>,
) -> Result<(StatusCode, Json<RuleJson>), ApiError> {
    let title = trimmed("title", &body.title, TITLE_MAX)?;
    let text = trimmed("body", &body.body, BODY_MAX)?;
    let path_glob = clean_glob(body.path_glob.as_deref())?;
    check_scope(&state, body.scope, body.repository_id).await?;

    let rule = rules::insert(
        &state.db,
        NewRule {
            scope: body.scope,
            repository_id: body.repository_id,
            title: &title,
            body: &text,
            path_glob: path_glob.as_deref(),
            severity: body.severity.unwrap_or(Severity::Warning),
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(rule.into())))
}

async fn update(
    _user: CurrentUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<UpdateRule>,
) -> Result<Json<RuleJson>, ApiError> {
    let patch = RulePatch {
        title: body
            .title
            .map(|t| trimmed("title", &t, TITLE_MAX))
            .transpose()?,
        body: body
            .body
            .map(|b| trimmed("body", &b, BODY_MAX))
            .transpose()?,
        path_glob: body
            .path_glob
            .map(|g| clean_glob(g.as_deref()))
            .transpose()?,
        severity: body.severity,
        enabled: body.enabled,
    };
    let rule = rules::update(&state.db, id, patch)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(rule.into()))
}

async fn remove(
    _user: CurrentUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    if rules::delete(&state.db, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}
