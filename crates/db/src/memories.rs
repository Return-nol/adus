use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, types::Json};
use uuid::Uuid;

use crate::{Result, rules::KnowledgeScope};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "memory_source", rename_all = "snake_case")]
pub enum MemorySource {
    Manual,
    PrComment,
    /// Learned from the discussion of a closed PR.
    PrLearning,
}

/// Hanya `active` yang masuk ke prompt review; `pending` dan `rejected` tidak pernah.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "memory_status", rename_all = "snake_case")]
pub enum MemoryStatus {
    /// Waiting for an admin to approve or reject it.
    Pending,
    Active,
    /// Turned down; kept so learning does not propose it again.
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "memory_kind", rename_all = "snake_case")]
pub enum MemoryKind {
    /// A team convention: follow it, flag violations.
    Convention,
    /// A case declared by design or won't fix.
    Exception,
    /// A problem this repo had before, with its fix.
    FixPattern,
}

/// A thread a memory was learned from; stored in `memories.evidence` (jsonb).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub url: String,
    pub pr_number: Option<i64>,
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
    pub status: MemoryStatus,
    pub kind: MemoryKind,
    pub context: Option<String>,
    pub reason: Option<String>,
    pub resolution: Option<String>,
    #[sqlx(json)]
    pub evidence: Vec<Evidence>,
    /// The memory this one replaces once it is active.
    pub supersedes: Option<Uuid>,
    /// Set when a replacement became active; a superseded memory no longer reaches prompts.
    pub superseded_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

pub struct NewMemory<'a> {
    pub scope: KnowledgeScope,
    pub repository_id: Option<Uuid>,
    pub content: &'a str,
    pub source: MemorySource,
    pub source_url: Option<&'a str>,
    pub created_by: Option<Uuid>,
    pub status: MemoryStatus,
    pub kind: MemoryKind,
    pub context: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub resolution: Option<&'a str>,
    pub evidence: &'a [Evidence],
    pub supersedes: Option<Uuid>,
}

/// Perubahan parsial; `None` = biarkan. Scope dan repo tidak bisa diubah setelah dibuat.
#[derive(Debug, Default)]
pub struct MemoryPatch {
    pub content: Option<String>,
    pub enabled: Option<bool>,
    pub kind: Option<MemoryKind>,
    pub context: Option<Option<String>>,
    pub reason: Option<Option<String>>,
    pub resolution: Option<Option<String>>,
}

// `email` bertipe citext; cast ke text supaya cocok dengan String.
const COLUMNS: &str = "id, scope, repository_id, content, source, source_url, created_by, \
                       (SELECT email::text FROM users WHERE users.id = memories.created_by) \
                         AS created_by_email, \
                       enabled, status, kind, context, reason, resolution, evidence, \
                       supersedes, superseded_by, created_at";

// Memory milik repo yang sudah dihapus (soft delete) dianggap tidak ada.
const ACTIVE_REPO: &str = "(repository_id IS NULL \
     OR repository_id IN (SELECT id FROM repositories WHERE deleted_at IS NULL))";

// Filter status untuk `list` / `list_all`; parameter NULL = semua kecuali `rejected`.
fn status_filter(param: &str) -> String {
    format!("(status = {param} OR ({param}::memory_status IS NULL AND status <> 'rejected'))")
}

fn kind_filter(param: &str) -> String {
    format!("(kind = {param} OR {param}::memory_kind IS NULL)")
}

/// An `active` memory that `supersedes` another retires it in the same statement.
pub async fn insert(pool: &PgPool, new: NewMemory<'_>) -> Result<Memory> {
    sqlx::query_as(&format!(
        "WITH inserted AS ( \
           INSERT INTO memories (scope, repository_id, content, source, source_url, created_by, \
           status, kind, context, reason, resolution, evidence, supersedes) \
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING {COLUMNS}), \
         replaced AS ( \
           UPDATE memories SET superseded_by = inserted.id FROM inserted \
           WHERE inserted.status = 'active' AND memories.id = inserted.supersedes \
             AND memories.superseded_by IS NULL) \
         SELECT * FROM inserted"
    ))
    .bind(new.scope)
    .bind(new.repository_id)
    .bind(new.content)
    .bind(new.source)
    .bind(new.source_url)
    .bind(new.created_by)
    .bind(new.status)
    .bind(new.kind)
    .bind(new.context)
    .bind(new.reason)
    .bind(new.resolution)
    .bind(Json(new.evidence))
    .bind(new.supersedes)
    .fetch_one(pool)
    .await
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<Memory>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM memories WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// `Some(repo)` = memory milik repo itu saja; `None` = memory global saja.
/// `status` `None` = semua status kecuali `rejected`.
pub async fn list(
    pool: &PgPool,
    repository_id: Option<Uuid>,
    status: Option<MemoryStatus>,
    kind: Option<MemoryKind>,
) -> Result<Vec<Memory>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM memories \
         WHERE repository_id IS NOT DISTINCT FROM $1 AND {ACTIVE_REPO} AND {} AND {} \
         ORDER BY created_at, id",
        status_filter("$2"),
        kind_filter("$3")
    ))
    .bind(repository_id)
    .bind(status)
    .bind(kind)
    .fetch_all(pool)
    .await
}

/// Semua memory: global dan semua repo aktif. `status` `None` = semua kecuali `rejected`.
pub async fn list_all(
    pool: &PgPool,
    status: Option<MemoryStatus>,
    kind: Option<MemoryKind>,
) -> Result<Vec<Memory>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM memories WHERE {ACTIVE_REPO} AND {} AND {} ORDER BY created_at, id",
        status_filter("$1"),
        kind_filter("$2")
    ))
    .bind(status)
    .bind(kind)
    .fetch_all(pool)
    .await
}

pub async fn update(pool: &PgPool, id: Uuid, patch: MemoryPatch) -> Result<Option<Memory>> {
    sqlx::query_as(&format!(
        "UPDATE memories SET \
           content = COALESCE($2, content), \
           enabled = COALESCE($3, enabled), \
           kind = COALESCE($4, kind), \
           context = CASE WHEN $5 THEN $6 ELSE context END, \
           reason = CASE WHEN $7 THEN $8 ELSE reason END, \
           resolution = CASE WHEN $9 THEN $10 ELSE resolution END \
         WHERE id = $1 RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(patch.content)
    .bind(patch.enabled)
    .bind(patch.kind)
    .bind(patch.context.is_some())
    .bind(patch.context.flatten())
    .bind(patch.reason.is_some())
    .bind(patch.reason.flatten())
    .bind(patch.resolution.is_some())
    .bind(patch.resolution.flatten())
    .fetch_optional(pool)
    .await
}

/// `pending` → `active`, sekaligus mengganti isi jika `content` diberikan.
/// The memory it `supersedes` gets `superseded_by` in the same statement.
/// `None` jika memory tidak ada atau tidak `pending`.
pub async fn approve(pool: &PgPool, id: Uuid, content: Option<&str>) -> Result<Option<Memory>> {
    sqlx::query_as(&format!(
        "WITH approved AS ( \
           UPDATE memories SET status = 'active', content = COALESCE($2, content) \
           WHERE id = $1 AND status = 'pending' RETURNING {COLUMNS}), \
         replaced AS ( \
           UPDATE memories SET superseded_by = approved.id FROM approved \
           WHERE memories.id = approved.supersedes AND memories.superseded_by IS NULL) \
         SELECT * FROM approved"
    ))
    .bind(id)
    .bind(content)
    .fetch_optional(pool)
    .await
}

/// `pending` → `rejected`. `None` jika memory tidak ada atau tidak `pending`.
pub async fn reject(pool: &PgPool, id: Uuid) -> Result<Option<Memory>> {
    sqlx::query_as(&format!(
        "UPDATE memories SET status = 'rejected' \
         WHERE id = $1 AND status = 'pending' RETURNING {COLUMNS}"
    ))
    .bind(id)
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
/// Hanya yang `enabled`, berstatus `active`, dan belum digantikan (`superseded_by`).
pub async fn applicable(pool: &PgPool, repository_id: Uuid) -> Result<Vec<Memory>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM memories \
         WHERE enabled AND status = 'active' AND superseded_by IS NULL \
           AND (repository_id IS NULL OR repository_id = $1) AND {ACTIVE_REPO} \
         ORDER BY created_at, id"
    ))
    .bind(repository_id)
    .fetch_all(pool)
    .await
}

/// Konteks untuk ekstraksi learning: memory global ditambah memory repo itu (jika repo masih aktif),
/// semua status, `enabled` atau tidak.
pub async fn for_learning(pool: &PgPool, repository_id: Uuid) -> Result<Vec<Memory>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM memories \
         WHERE (repository_id IS NULL OR repository_id = $1) AND {ACTIVE_REPO} \
         ORDER BY created_at, id"
    ))
    .bind(repository_id)
    .fetch_all(pool)
    .await
}
