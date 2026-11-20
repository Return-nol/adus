use adus_core::types::{Severity, normalize_path};
use chrono::{DateTime, Utc};
use globset::Glob;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::Result;

/// Dipakai bersama oleh `rules` dan `memories`: global (tanpa repo) atau per repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "knowledge_scope", rename_all = "snake_case")]
pub enum KnowledgeScope {
    Global,
    Repo,
}

#[derive(Debug, Clone, FromRow)]
pub struct Rule {
    pub id: Uuid,
    pub scope: KnowledgeScope,
    pub repository_id: Option<Uuid>,
    pub title: String,
    pub body: String,
    pub path_glob: Option<String>,
    pub severity: Severity,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct NewRule<'a> {
    pub scope: KnowledgeScope,
    pub repository_id: Option<Uuid>,
    pub title: &'a str,
    pub body: &'a str,
    pub path_glob: Option<&'a str>,
    pub severity: Severity,
}

/// Perubahan parsial. `None` = biarkan; `path_glob: Some(None)` = kosongkan.
/// Scope dan repo tidak bisa diubah setelah dibuat.
#[derive(Debug, Default)]
pub struct RulePatch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub path_glob: Option<Option<String>>,
    pub severity: Option<Severity>,
    pub enabled: Option<bool>,
}

const COLUMNS: &str = "id, scope, repository_id, title, body, path_glob, severity, enabled, \
                       created_at, updated_at";

// Rule milik repo yang sudah dihapus (soft delete) dianggap tidak ada.
const ACTIVE_REPO: &str = "(repository_id IS NULL \
     OR repository_id IN (SELECT id FROM repositories WHERE deleted_at IS NULL))";

pub async fn insert(pool: &PgPool, new: NewRule<'_>) -> Result<Rule> {
    sqlx::query_as(&format!(
        "INSERT INTO rules (scope, repository_id, title, body, path_glob, severity) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING {COLUMNS}"
    ))
    .bind(new.scope)
    .bind(new.repository_id)
    .bind(new.title)
    .bind(new.body)
    .bind(new.path_glob)
    .bind(new.severity)
    .fetch_one(pool)
    .await
}

/// `Some(repo)` = rule milik repo itu saja; `None` = rule global saja.
pub async fn list(pool: &PgPool, repository_id: Option<Uuid>) -> Result<Vec<Rule>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM rules \
         WHERE repository_id IS NOT DISTINCT FROM $1 AND {ACTIVE_REPO} \
         ORDER BY created_at, id"
    ))
    .bind(repository_id)
    .fetch_all(pool)
    .await
}

/// Semua rule: global dan semua repo aktif.
pub async fn list_all(pool: &PgPool) -> Result<Vec<Rule>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM rules WHERE {ACTIVE_REPO} ORDER BY created_at, id"
    ))
    .fetch_all(pool)
    .await
}

pub async fn update(pool: &PgPool, id: Uuid, patch: RulePatch) -> Result<Option<Rule>> {
    // $4 menandai apakah path_glob diubah (termasuk dikosongkan); $5 nilai barunya.
    sqlx::query_as(&format!(
        "UPDATE rules SET \
           title = COALESCE($2, title), \
           body = COALESCE($3, body), \
           path_glob = CASE WHEN $4 THEN $5 ELSE path_glob END, \
           severity = COALESCE($6, severity), \
           enabled = COALESCE($7, enabled), \
           updated_at = now() \
         WHERE id = $1 RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(patch.title)
    .bind(patch.body)
    .bind(patch.path_glob.is_some())
    .bind(patch.path_glob.flatten())
    .bind(patch.severity)
    .bind(patch.enabled)
    .fetch_optional(pool)
    .await
}

/// `false` jika rule tidak ada.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool> {
    let done = sqlx::query("DELETE FROM rules WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Rule aktif untuk satu repo: semua rule global ditambah rule repo itu (jika repo masih aktif).
pub async fn applicable(pool: &PgPool, repository_id: Uuid) -> Result<Vec<Rule>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM rules \
         WHERE enabled AND (repository_id IS NULL OR repository_id = $1) AND {ACTIVE_REPO} \
         ORDER BY created_at, id"
    ))
    .bind(repository_id)
    .fetch_all(pool)
    .await
}

/// Glob yang bisa dikompilasi oleh `globset`.
pub fn validate_glob(glob: &str) -> bool {
    Glob::new(glob).is_ok()
}

/// Apakah rule berlaku untuk file ini. Tanpa `path_glob` = semua file. Path dinormalisasi
/// (tanpa `/` di depan), jadi `src/**` cocok dengan `/src/a/b.rs` dari Azure.
/// Glob tidak valid = tidak cocok (dan diberi peringatan).
pub fn rule_matches_path(rule: &Rule, path: &str) -> bool {
    let Some(glob) = &rule.path_glob else {
        return true;
    };
    // ponytail: glob dikompilasi tiap panggilan; cache per review jika jumlah rule x file membesar.
    match Glob::new(glob) {
        Ok(g) => g.compile_matcher().is_match(normalize_path(path)),
        Err(err) => {
            tracing::warn!("rule {} has an invalid path_glob {glob:?}: {err}", rule.id);
            false
        }
    }
}
