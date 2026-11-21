use adus_core::types::ProviderKind;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "ai_key_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum AiKeyStatus {
    Ok,
    Invalid,
    RateLimited,
}

#[derive(Debug, Clone, FromRow)]
pub struct AiProvider {
    pub id: Uuid,
    pub kind: ProviderKind,
    pub name: String,
    pub base_url: Option<String>,
    pub primary_model: String,
    pub fast_model: Option<String>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

impl AiProvider {
    pub fn effective_fast_model(&self) -> &str {
        self.fast_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or(&self.primary_model)
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct AiKey {
    pub id: Uuid,
    pub provider_id: Uuid,
    pub label: String,
    pub encrypted_key: String,
    pub key_last4: String,
    pub enabled: bool,
    pub status: AiKeyStatus,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Perubahan parsial key. `None` = biarkan. Mengganti rahasia (`secret` = terenkripsi + last4)
/// mengembalikan key ke status `ok` dan menghapus cooldown: key `invalid` pulih lewat edit.
#[derive(Debug, Default)]
pub struct KeyPatch {
    pub label: Option<String>,
    pub enabled: Option<bool>,
    pub secret: Option<(String, String)>,
}

const PROVIDER_COLUMNS: &str =
    "id, kind, name, base_url, primary_model, fast_model, enabled, created_at";
const KEY_COLUMNS: &str = "id, provider_id, label, encrypted_key, key_last4, enabled, status, \
                           cooldown_until, last_used_at, created_at";

pub async fn insert_provider(
    pool: &PgPool,
    kind: ProviderKind,
    name: &str,
    base_url: Option<&str>,
    primary_model: &str,
    fast_model: Option<&str>,
) -> Result<AiProvider> {
    sqlx::query_as(&format!(
        "INSERT INTO ai_providers (kind, name, base_url, primary_model, fast_model) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING {PROVIDER_COLUMNS}"
    ))
    .bind(kind)
    .bind(name)
    .bind(base_url)
    .bind(primary_model)
    .bind(fast_model)
    .fetch_one(pool)
    .await
}

pub async fn list_providers(pool: &PgPool) -> Result<Vec<AiProvider>> {
    sqlx::query_as(&format!(
        "SELECT {PROVIDER_COLUMNS} FROM ai_providers ORDER BY created_at, id"
    ))
    .fetch_all(pool)
    .await
}

pub async fn get_provider(pool: &PgPool, id: Uuid) -> Result<Option<AiProvider>> {
    sqlx::query_as(&format!(
        "SELECT {PROVIDER_COLUMNS} FROM ai_providers WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// `kind` tidak bisa diubah (key yang tersimpan terikat ke jenis provider-nya).
pub async fn update_provider(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    base_url: Option<&str>,
    primary_model: &str,
    fast_model: Option<&str>,
    enabled: bool,
) -> Result<Option<AiProvider>> {
    sqlx::query_as(&format!(
        "UPDATE ai_providers SET name = $2, base_url = $3, primary_model = $4, fast_model = $5, \
         enabled = $6 WHERE id = $1 \
         RETURNING {PROVIDER_COLUMNS}"
    ))
    .bind(id)
    .bind(name)
    .bind(base_url)
    .bind(primary_model)
    .bind(fast_model)
    .bind(enabled)
    .fetch_optional(pool)
    .await
}

/// Key ikut terhapus; repo/settings/usage yang menunjuk provider ini di-NULL-kan oleh FK.
/// `true` jika ada baris yang dihapus.
pub async fn delete_provider(pool: &PgPool, id: Uuid) -> Result<bool> {
    let done = sqlx::query("DELETE FROM ai_providers WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

pub async fn insert_key(
    pool: &PgPool,
    provider_id: Uuid,
    label: &str,
    encrypted_key: &str,
    key_last4: &str,
) -> Result<AiKey> {
    sqlx::query_as(&format!(
        "INSERT INTO ai_keys (provider_id, label, encrypted_key, key_last4) \
         VALUES ($1, $2, $3, $4) RETURNING {KEY_COLUMNS}"
    ))
    .bind(provider_id)
    .bind(label)
    .bind(encrypted_key)
    .bind(key_last4)
    .fetch_one(pool)
    .await
}

pub async fn get_key(pool: &PgPool, id: Uuid) -> Result<Option<AiKey>> {
    sqlx::query_as(&format!("SELECT {KEY_COLUMNS} FROM ai_keys WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Dipakai settings (provider default) atau repo aktif (override review)? Menghapusnya akan
/// diam-diam mengosongkan pilihan itu lewat FK, jadi API menolak dulu.
pub async fn provider_in_use(pool: &PgPool, id: Uuid) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM settings WHERE default_provider_id = $1)              OR EXISTS (SELECT 1 FROM repositories                         WHERE review_provider_id = $1 AND deleted_at IS NULL)",
    )
    .bind(id)
    .fetch_one(pool)
    .await
}

pub async fn list_keys(pool: &PgPool, provider_id: Uuid) -> Result<Vec<AiKey>> {
    sqlx::query_as(&format!(
        "SELECT {KEY_COLUMNS} FROM ai_keys WHERE provider_id = $1 ORDER BY created_at, id"
    ))
    .bind(provider_id)
    .fetch_all(pool)
    .await
}

pub async fn update_key(pool: &PgPool, id: Uuid, patch: KeyPatch) -> Result<Option<AiKey>> {
    let (encrypted, last4) = match patch.secret {
        Some((e, l)) => (Some(e), Some(l)),
        None => (None, None),
    };
    sqlx::query_as(&format!(
        "UPDATE ai_keys SET \
           label = COALESCE($2, label), \
           enabled = COALESCE($3, enabled), \
           encrypted_key = COALESCE($4, encrypted_key), \
           key_last4 = COALESCE($5, key_last4), \
           status = CASE WHEN $4::text IS NOT NULL THEN 'ok'::ai_key_status ELSE status END, \
           cooldown_until = CASE WHEN $4::text IS NOT NULL THEN NULL ELSE cooldown_until END \
         WHERE id = $1 RETURNING {KEY_COLUMNS}"
    ))
    .bind(id)
    .bind(patch.label)
    .bind(patch.enabled)
    .bind(encrypted)
    .bind(last4)
    .fetch_optional(pool)
    .await
}

pub async fn delete_key(pool: &PgPool, id: Uuid) -> Result<bool> {
    let done = sqlx::query("DELETE FROM ai_keys WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Key yang boleh dicoba sekarang, paling lama tidak dipakai lebih dulu (round-robin).
/// Provider yang dinonaktifkan tidak menghasilkan key.
pub async fn candidate_keys(
    pool: &PgPool,
    provider_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Vec<AiKey>> {
    sqlx::query_as(&format!(
        "SELECT {KEY_COLUMNS} FROM ai_keys \
         WHERE provider_id = $1 AND enabled AND status <> 'invalid' \
           AND (cooldown_until IS NULL OR cooldown_until <= $2) \
           AND EXISTS (SELECT 1 FROM ai_providers p WHERE p.id = ai_keys.provider_id AND p.enabled) \
         ORDER BY last_used_at NULLS FIRST, id"
    ))
    .bind(provider_id)
    .bind(now)
    .fetch_all(pool)
    .await
}

/// Panggilan sukses. Key `invalid` tidak disentuh (hanya edit yang memulihkannya). Key kembali
/// `ok` hanya jika cooldown-nya kosong atau sudah lewat; cooldown yang dipasang worker lain
/// (karena 429) tidak dihapus oleh sukses yang datang bersamaan.
pub async fn mark_used(pool: &PgPool, key_id: Uuid, now: DateTime<Utc>) -> Result<()> {
    sqlx::query(
        "UPDATE ai_keys SET last_used_at = $2,            status = CASE WHEN cooldown_until IS NULL OR cooldown_until <= $2                          THEN 'ok'::ai_key_status ELSE status END,            cooldown_until = CASE WHEN cooldown_until IS NULL OR cooldown_until <= $2                                  THEN NULL ELSE cooldown_until END          WHERE id = $1 AND status <> 'invalid'",
    )
    .bind(key_id)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Key `invalid` tetap `invalid`; cooldown yang sudah ada tidak diperpendek.
pub async fn mark_cooldown(pool: &PgPool, key_id: Uuid, until: DateTime<Utc>) -> Result<()> {
    sqlx::query(
        "UPDATE ai_keys SET status = 'rate_limited',            cooldown_until = GREATEST(COALESCE(cooldown_until, $2), $2)          WHERE id = $1 AND status <> 'invalid'",
    )
    .bind(key_id)
    .bind(until)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_invalid(pool: &PgPool, key_id: Uuid) -> Result<()> {
    sqlx::query("UPDATE ai_keys SET status = 'invalid' WHERE id = $1")
        .bind(key_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "../../migrations")]
    async fn provider_models_crud_optional_fast_model_and_effective_fallback(pool: PgPool) {
        let created = insert_provider(
            &pool,
            ProviderKind::Anthropic,
            "Anthropic",
            None,
            "claude-sonnet-4-5",
            None,
        )
        .await
        .unwrap();
        assert_eq!(created.primary_model, "claude-sonnet-4-5");
        assert_eq!(created.fast_model, None);
        assert_eq!(created.effective_fast_model(), "claude-sonnet-4-5");

        let with_fast = update_provider(
            &pool,
            created.id,
            "Anthropic",
            None,
            "claude-sonnet-4-5",
            Some("claude-haiku-4-5"),
            true,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(with_fast.fast_model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(with_fast.effective_fast_model(), "claude-haiku-4-5");

        let cleared = update_provider(
            &pool,
            created.id,
            "Anthropic",
            None,
            "claude-sonnet-4-5",
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(cleared.fast_model, None);
        assert_eq!(cleared.effective_fast_model(), "claude-sonnet-4-5");
    }
}
