use adus_db::{
    memories::{self, Memory, MemoryPatch, MemorySource, MemoryStatus, NewMemory},
    rules::KnowledgeScope,
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

use super::rules::{ListQuery, check_repo, check_scope, trimmed};
use crate::{
    auth::CurrentUser,
    error::{ApiError, ApiJson, ApiPath, ApiQuery},
    state::AppState,
};

const CONTENT_MAX: usize = 2000;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/memories", get(list).post(create))
        .route("/memories/{id}", patch(update).delete(remove))
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
            created_at: m.created_at,
        }
    }
}

async fn list(
    _user: CurrentUser,
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<MemoryJson>>, ApiError> {
    let found = if query.all {
        memories::list_all(&state.db, None).await?
    } else {
        check_repo(&state, query.repository_id).await?;
        memories::list(&state.db, query.repository_id, None).await?
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
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(memory.into())))
}

async fn update(
    _user: CurrentUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<UpdateMemory>,
) -> Result<Json<MemoryJson>, ApiError> {
    let patch = MemoryPatch {
        content: body
            .content
            .map(|c| trimmed("content", &c, CONTENT_MAX))
            .transpose()?,
        enabled: body.enabled,
    };
    let memory = memories::update(&state.db, id, patch)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(memory.into()))
}

async fn remove(
    _user: CurrentUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    if memories::delete(&state.db, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}
