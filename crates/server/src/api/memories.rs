use adus_db::{
    memories::{
        self, Evidence, Memory, MemoryKind, MemoryPatch, MemorySource, MemoryStatus, NewMemory,
    },
    rules::KnowledgeScope,
    users::{Role, User},
};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, patch, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    repositories::present_or_null,
    rules::{check_repo, check_scope, trimmed},
};
use crate::{
    auth::{AdminUser, CurrentUser},
    error::{ApiError, ApiJson, ApiPath, ApiQuery},
    state::AppState,
};

const CONTENT_MAX: usize = 2000;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/memories", get(list).post(create))
        .route("/memories/{id}", patch(update).delete(remove))
        .route("/memories/{id}/approve", post(approve))
        .route("/memories/{id}/reject", post(reject))
}

#[derive(Deserialize)]
struct ListQuery {
    repository_id: Option<Uuid>,
    /// `true` = semua memory (global dan semua repo), mengabaikan `repository_id`.
    #[serde(default)]
    all: bool,
    /// String, bukan enum: nilai tak dikenal harus 422, bukan 400 dari `ApiQuery`.
    status: Option<String>,
    /// String, bukan enum: nilai tak dikenal harus 422, bukan 400 dari `ApiQuery`.
    kind: Option<String>,
}

#[derive(Deserialize)]
struct ApproveMemory {
    content: Option<String>,
}

/// `source`, `source_url` dan `created_by` sengaja tidak ada: server yang mengisinya.
#[derive(Deserialize)]
struct CreateMemory {
    scope: KnowledgeScope,
    repository_id: Option<Uuid>,
    content: String,
    kind: Option<MemoryKind>,
    context: Option<String>,
    reason: Option<String>,
    resolution: Option<String>,
}

#[derive(Deserialize)]
struct UpdateMemory {
    content: Option<String>,
    enabled: Option<bool>,
    kind: Option<MemoryKind>,
    /// Ada dan `null` (atau kosong) = hapus; tidak ada = biarkan.
    #[serde(default, deserialize_with = "present_or_null")]
    context: Option<Option<String>>,
    /// Ada dan `null` (atau kosong) = hapus; tidak ada = biarkan.
    #[serde(default, deserialize_with = "present_or_null")]
    reason: Option<Option<String>>,
    /// Ada dan `null` (atau kosong) = hapus; tidak ada = biarkan.
    #[serde(default, deserialize_with = "present_or_null")]
    resolution: Option<Option<String>>,
}

#[derive(Serialize)]
struct CreatorJson {
    id: Uuid,
    email: String,
}

#[derive(Serialize)]
struct MemoryJson {
    id: Uuid,
    scope: KnowledgeScope,
    repository_id: Option<Uuid>,
    content: String,
    source: MemorySource,
    source_url: Option<String>,
    created_by: Option<CreatorJson>,
    enabled: bool,
    status: MemoryStatus,
    kind: MemoryKind,
    context: Option<String>,
    reason: Option<String>,
    resolution: Option<String>,
    evidence: Vec<Evidence>,
    supersedes: Option<Uuid>,
    superseded_by: Option<Uuid>,
    created_at: DateTime<Utc>,
}

impl From<Memory> for MemoryJson {
    fn from(m: Memory) -> MemoryJson {
        MemoryJson {
            id: m.id,
            scope: m.scope,
            repository_id: m.repository_id,
            content: m.content,
            source: m.source,
            source_url: m.source_url,
            created_by: m
                .created_by
                .zip(m.created_by_email)
                .map(|(id, email)| CreatorJson { id, email }),
            enabled: m.enabled,
            status: m.status,
            kind: m.kind,
            context: m.context,
            reason: m.reason,
            resolution: m.resolution,
            evidence: m.evidence,
            supersedes: m.supersedes,
            superseded_by: m.superseded_by,
            created_at: m.created_at,
        }
    }
}

fn optional_trimmed(
    field: &str,
    raw: Option<&str>,
    max: usize,
) -> Result<Option<String>, ApiError> {
    let Some(value) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if value.chars().count() > max {
        return Err(ApiError::validation(format!(
            "{field} must be between 1 and {max} characters"
        )));
    }
    Ok(Some(value.to_string()))
}

fn patch_optional_text(
    field: &str,
    raw: Option<Option<String>>,
    max: usize,
) -> Result<Option<Option<String>>, ApiError> {
    raw.map(|inner| optional_trimmed(field, inner.as_deref(), max))
        .transpose()
}

async fn list(
    _user: CurrentUser,
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<MemoryJson>>, ApiError> {
    let status = match query.status.as_deref() {
        None => None,
        Some("pending") => Some(MemoryStatus::Pending),
        Some("active") => Some(MemoryStatus::Active),
        Some("rejected") => Some(MemoryStatus::Rejected),
        Some(_) => {
            return Err(ApiError::validation(
                "status must be pending, active or rejected",
            ));
        }
    };
    let kind = match query.kind.as_deref() {
        None => None,
        Some("convention") => Some(MemoryKind::Convention),
        Some("exception") => Some(MemoryKind::Exception),
        Some("fix_pattern") => Some(MemoryKind::FixPattern),
        Some(_) => {
            return Err(ApiError::validation(
                "kind must be convention, exception or fix_pattern",
            ));
        }
    };
    let found = if query.all {
        memories::list_all(&state.db, status, kind).await?
    } else {
        check_repo(&state, query.repository_id).await?;
        memories::list(&state.db, query.repository_id, status, kind).await?
    };
    Ok(Json(found.into_iter().map(MemoryJson::from).collect()))
}

async fn create(
    CurrentUser(user): CurrentUser,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateMemory>,
) -> Result<(StatusCode, Json<MemoryJson>), ApiError> {
    let content = trimmed("content", &body.content, CONTENT_MAX)?;
    let context = optional_trimmed("context", body.context.as_deref(), CONTENT_MAX)?;
    let reason = optional_trimmed("reason", body.reason.as_deref(), CONTENT_MAX)?;
    let resolution = optional_trimmed("resolution", body.resolution.as_deref(), CONTENT_MAX)?;
    check_scope(&state, body.scope, body.repository_id).await?;

    let memory = memories::insert(
        &state.db,
        NewMemory {
            scope: body.scope,
            repository_id: body.repository_id,
            content: &content,
            source: MemorySource::Manual,
            source_url: None,
            created_by: Some(user.id),
            status: MemoryStatus::Active,
            kind: body.kind.unwrap_or(MemoryKind::Convention),
            context: context.as_deref(),
            reason: reason.as_deref(),
            resolution: resolution.as_deref(),
            evidence: &[],
            supersedes: None,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(memory.into())))
}

/// Memory `pending` / `rejected` hanya boleh diubah atau dihapus admin.
async fn check_editable(state: &AppState, user: &User, id: Uuid) -> Result<(), ApiError> {
    let memory = memories::get(&state.db, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if memory.status != MemoryStatus::Active && user.role != Role::Admin {
        return Err(ApiError::forbidden());
    }
    Ok(())
}

/// `None` dari DB: memory tidak ada (404) atau tidak `pending` (409).
async fn transition_result(
    state: &AppState,
    id: Uuid,
    changed: Option<Memory>,
) -> Result<Json<MemoryJson>, ApiError> {
    if let Some(memory) = changed {
        return Ok(Json(memory.into()));
    }
    if memories::get(&state.db, id).await?.is_none() {
        return Err(ApiError::not_found());
    }
    Err(ApiError::conflict(
        "invalid_status",
        "only a pending memory can be approved or rejected",
    ))
}

async fn approve(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<ApproveMemory>,
) -> Result<Json<MemoryJson>, ApiError> {
    let content = body
        .content
        .map(|c| trimmed("content", &c, CONTENT_MAX))
        .transpose()?;
    let changed = memories::approve(&state.db, id, content.as_deref()).await?;
    transition_result(&state, id, changed).await
}

async fn reject(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<MemoryJson>, ApiError> {
    let changed = memories::reject(&state.db, id).await?;
    transition_result(&state, id, changed).await
}

async fn update(
    CurrentUser(user): CurrentUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<UpdateMemory>,
) -> Result<Json<MemoryJson>, ApiError> {
    check_editable(&state, &user, id).await?;
    let patch = MemoryPatch {
        content: body
            .content
            .map(|c| trimmed("content", &c, CONTENT_MAX))
            .transpose()?,
        enabled: body.enabled,
        kind: body.kind,
        context: patch_optional_text("context", body.context, CONTENT_MAX)?,
        reason: patch_optional_text("reason", body.reason, CONTENT_MAX)?,
        resolution: patch_optional_text("resolution", body.resolution, CONTENT_MAX)?,
    };
    let memory = memories::update(&state.db, id, patch)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(memory.into()))
}

async fn remove(
    CurrentUser(user): CurrentUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    check_editable(&state, &user, id).await?;
    if memories::delete(&state.db, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}
