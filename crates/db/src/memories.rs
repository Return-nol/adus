use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::{Result, rules::KnowledgeScope};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "memory_source", rename_all = "snake_case")]
pub enum MemorySource {
    Manual,
    PrComment,
}

#[derive(Debug, Clone, FromRow)]
pub struct Memory {
    pub id: Uuid,
    pub scope: KnowledgeScope,
    pub repository_id: Option<Uuid>,
    pub content: String,
    pub source: MemorySource,
    pub source_url: Option<String>,
    pub created_by: Option<Uuid>,
    /// Email pembuat, untuk ditampilkan di UI; `None` jika user sudah dihapus.
    pub created_by_email: Option<String>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

pub struct NewMemory<'a> {
    pub scope: KnowledgeScope,
    pub repository_id: Option<Uuid>,
    pub content: &'a str,
    pub source: MemorySource,
    pub source_url: Option<&'a str>,
    pub created_by: Option<Uuid>,
}

/// Perubahan parsial; `None` = biarkan. Scope dan repo tidak bisa diubah setelah dibuat.
#[derive(Debug, Default)]
pub struct MemoryPatch {
    pub content: Option<String>,
    pub enabled: Option<bool>,
}

// `email` bertipe citext; cast ke text supaya cocok dengan String.
const COLUMNS: &str = "id, scope, repository_id, content, source, source_url, created_by, \
                       (SELECT email::text FROM users WHERE users.id = memories.created_by) \
                         AS created_by_email, \
                       enabled, created_at";

// Memory milik repo yang sudah dihapus (soft delete) dianggap tidak ada.
const ACTIVE_REPO: &str = "(repository_id IS NULL \
     OR repository_id IN (SELECT id FROM repositories WHERE deleted_at IS NULL))";

pub async fn insert(pool: &PgPool, new: NewMemory<'_>) -> Result<Memory> {
    sqlx::query_as(&format!(
        "INSERT INTO memories (scope, repository_id, content, source, source_url, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING {COLUMNS}"
    ))
    .bind(new.scope)
    .bind(new.repository_id)
    .bind(new.content)
    .bind(new.source)
    .bind(new.source_url)
    .bind(new.created_by)
    .fetch_one(pool)
    .await
}

/// `Some(repo)` = memory milik repo itu saja; `None` = memory global saja.
pub async fn list(pool: &PgPool, repository_id: Option<Uuid>) -> Result<Vec<Memory>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM memories \
         WHERE repository_id IS NOT DISTINCT FROM $1 AND {ACTIVE_REPO} \
         ORDER BY created_at, id"
    ))
    .bind(repository_id)
    .fetch_all(pool)
    .await
}

/// Semua memory: global dan semua repo aktif.
pub async fn list_all(pool: &PgPool) -> Result<Vec<Memory>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM memories WHERE {ACTIVE_REPO} ORDER BY created_at, id"
    ))
    .fetch_all(pool)
    .await
}

pub async fn update(pool: &PgPool, id: Uuid, patch: MemoryPatch) -> Result<Option<Memory>> {
    sqlx::query_as(&format!(
        "UPDATE memories SET \
           content = COALESCE($2, content), \
           enabled = COALESCE($3, enabled) \
         WHERE id = $1 RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(patch.content)
    .bind(patch.enabled)
    .fetch_optional(pool)
    .await
}

/// `false` jika memory tidak ada.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool> {
    let done = sqlx::query("DELETE FROM memories WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Memory aktif untuk satu repo: semua memory global ditambah memory repo itu (jika repo masih aktif).
pub async fn applicable(pool: &PgPool, repository_id: Uuid) -> Result<Vec<Memory>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM memories \
         WHERE enabled AND (repository_id IS NULL OR repository_id = $1) AND {ACTIVE_REPO} \
         ORDER BY created_at, id"
    ))
    .bind(repository_id)
    .fetch_all(pool)
    .await
}
