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
    /// `None` only for providers that had no model before migration 0008: they need setup
    /// and are treated as "no model configured" until an admin picks one.
    pub primary_model: Option<String>,
    pub fast_model: Option<String>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

impl AiProvider {
    /// The model for the main review pass, or `None` when the provider still needs one.
    pub fn primary_model(&self) -> Option<&str> {
        non_blank(self.primary_model.as_deref())
    }

    /// The optional fast model configured on this provider, if non-blank.
    pub fn fast_model(&self) -> Option<&str> {
        non_blank(self.fast_model.as_deref())
    }

    /// The model for cheaper follow-up work: `fast_model`, else the primary model.
    pub fn effective_fast_model(&self) -> Option<&str> {
        self.fast_model().or_else(|| self.primary_model())
    }
}

fn non_blank(model: Option<&str>) -> Option<&str> {
    model.map(str::trim).filter(|m| !m.is_empty())
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
    primary_model: Option<&str>,
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
        assert_eq!(created.primary_model(), Some("claude-sonnet-4-5"));
        assert_eq!(created.fast_model, None);
        assert_eq!(created.effective_fast_model(), Some("claude-sonnet-4-5"));

        let with_fast = update_provider(
            &pool,
            created.id,
            "Anthropic",
            None,
            Some("claude-sonnet-4-5"),
            Some("claude-haiku-4-5"),
            true,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(with_fast.fast_model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(with_fast.effective_fast_model(), Some("claude-haiku-4-5"));

        let cleared = update_provider(
            &pool,
            created.id,
            "Anthropic",
            None,
            Some("claude-sonnet-4-5"),
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(cleared.fast_model, None);
        assert_eq!(cleared.effective_fast_model(), Some("claude-sonnet-4-5"));
    }

    #[test]
    fn provider_without_primary_model_has_no_model_at_all() {
        let provider = AiProvider {
            id: Uuid::nil(),
            kind: ProviderKind::Openai,
            name: "needs setup".into(),
            base_url: None,
            primary_model: None,
            fast_model: Some("  ".into()),
            enabled: true,
            created_at: Utc::now(),
        };
        assert_eq!(provider.primary_model(), None);
        assert_eq!(provider.effective_fast_model(), None);
    }

    /// Runs migrations 1–7, seeds an install that still has `settings.default_model` and
    /// `repositories.review_model`, then runs migration 8 and checks the backfill.
    #[sqlx::test(migrations = false)]
    async fn migration_8_backfills_provider_models_without_inventing_one(pool: PgPool) {
        for m in crate::MIGRATOR.iter().filter(|m| m.version < 8) {
            sqlx::raw_sql(&m.sql).execute(&pool).await.unwrap();
        }
        let seed = r#"
            INSERT INTO ai_providers (id, kind, name) VALUES
                ('00000000-0000-0000-0000-00000000000a', 'openai', 'global default'),
                ('00000000-0000-0000-0000-00000000000b', 'anthropic', 'repo override'),
                ('00000000-0000-0000-0000-00000000000c', 'gemini', 'never used'),
                ('00000000-0000-0000-0000-00000000000d', 'openai', 'only a deleted repo');
            UPDATE settings SET default_provider_id = '00000000-0000-0000-0000-00000000000a',
                                default_model = ' gpt-5 ' WHERE id = 1;
            INSERT INTO connections (id, kind, name, org_or_owner, encrypted_token, token_last4)
                VALUES ('00000000-0000-0000-0000-000000000001', 'github', 'c', 'o', 'x', 'xxxx');
            INSERT INTO repositories (id, connection_id, external_id, owner, name,
                                      review_provider_id, review_model, created_at, deleted_at) VALUES
                ('00000000-0000-0000-0000-000000000011', '00000000-0000-0000-0000-000000000001',
                 'r1', 'o', 'old', '00000000-0000-0000-0000-00000000000b', 'claude-old',
                 now() - interval '2 days', NULL),
                ('00000000-0000-0000-0000-000000000012', '00000000-0000-0000-0000-000000000001',
                 'r2', 'o', 'new', '00000000-0000-0000-0000-00000000000b', 'claude-new',
                 now() - interval '1 day', NULL),
                ('00000000-0000-0000-0000-000000000013', '00000000-0000-0000-0000-000000000001',
                 'r3', 'o', 'gone', '00000000-0000-0000-0000-00000000000d', 'deleted-model',
                 now(), now()),
                ('00000000-0000-0000-0000-000000000014', '00000000-0000-0000-0000-000000000001',
                 'r4', 'o', 'provider only', '00000000-0000-0000-0000-00000000000c', NULL,
                 now(), NULL),
                ('00000000-0000-0000-0000-000000000015', '00000000-0000-0000-0000-000000000001',
                 'r5', 'o', 'provider only, provider has a model',
                 '00000000-0000-0000-0000-00000000000b', '  ', now(), NULL);
        "#;
        sqlx::raw_sql(seed).execute(&pool).await.unwrap();

        let m8 = crate::MIGRATOR.iter().find(|m| m.version == 8).unwrap();
        sqlx::raw_sql(&m8.sql).execute(&pool).await.unwrap();

        let model = |id: u128| {
            let pool = pool.clone();
            async move {
                get_provider(&pool, Uuid::from_u128(id))
                    .await
                    .unwrap()
                    .unwrap()
                    .primary_model
            }
        };
        assert_eq!(model(0xa).await.as_deref(), Some("gpt-5"));
        assert_eq!(model(0xb).await.as_deref(), Some("claude-new"));
        assert_eq!(model(0xc).await, None, "no model is invented");
        assert_eq!(model(0xd).await, None, "deleted repos are ignored");

        let overrides: Vec<(String, Option<Uuid>)> = sqlx::query_as(
            "SELECT external_id, review_provider_id FROM repositories ORDER BY external_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            overrides,
            vec![
                ("r1".into(), Some(Uuid::from_u128(0xb))),
                ("r2".into(), Some(Uuid::from_u128(0xb))),
                ("r3".into(), Some(Uuid::from_u128(0xd))),
                // Provider-only overrides used the global provider+model before: still do.
                ("r4".into(), None),
                ("r5".into(), None),
            ]
        );
    }
}
