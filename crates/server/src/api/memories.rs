use adus_db::{
    memories::{self, Memory, MemoryKind, MemoryPatch, MemorySource, MemoryStatus, NewMemory},
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

use super::rules::{check_repo, check_scope, trimmed};
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
}

#[derive(Deserialize)]
struct UpdateMemory {
    content: Option<String>,
    enabled: Option<bool>,
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
            created_at: m.created_at,
        }
    }
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
    let found = if query.all {
        memories::list_all(&state.db, status).await?
    } else {
        check_repo(&state, query.repository_id).await?;
        memories::list(&state.db, query.repository_id, status).await?
    };
    Ok(Json(found.into_iter().map(MemoryJson::from).collect()))
}

async fn create(
    CurrentUser(user): CurrentUser,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateMemory>,
) -> Result<(StatusCode, Json<MemoryJson>), ApiError> {
    let content = trimmed("content", &body.content, CONTENT_MAX)?;
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
            kind: MemoryKind::Convention,
            context: None,
            reason: None,
            resolution: None,
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
        ..Default::default()
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
