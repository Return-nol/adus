use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::{Result, repositories};

/// Satu baris per percobaan panggilan LLM (sukses maupun gagal).
pub struct NewUsage<'a> {
    pub review_id: Option<Uuid>,
    pub repository_id: Option<Uuid>,
    pub provider_id: Uuid,
    pub key_id: Uuid,
    pub model: &'a str,
    pub input_tokens: i32,
    pub output_tokens: i32,
    /// Part of `input_tokens` read from the provider's prompt cache.
    pub cache_read_tokens: i32,
    /// Part of `input_tokens` written to the provider's prompt cache.
    pub cache_write_tokens: i32,
    pub latency_ms: i32,
    pub success: bool,
    pub error_kind: Option<&'a str>,
}

pub async fn insert(pool: &PgPool, new: NewUsage<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO llm_usage \
         (review_id, repository_id, provider_id, key_id, model, input_tokens, output_tokens, \
          cache_read_tokens, cache_write_tokens, latency_ms, success, error_kind) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(new.review_id)
    .bind(new.repository_id)
    .bind(new.provider_id)
    .bind(new.key_id)
    .bind(new.model)
    .bind(new.input_tokens)
    .bind(new.output_tokens)
    .bind(new.cache_read_tokens)
    .bind(new.cache_write_tokens)
    .bind(new.latency_ms)
    .bind(new.success)
    .bind(new.error_kind)
    .execute(pool)
    .await?;
    Ok(())
}

/// Satu hari kalender UTC; hari tanpa panggilan berisi nol.
#[derive(Debug, Serialize, FromRow)]
pub struct DailyUsage {
    pub date: NaiveDate,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub calls: i64,
    pub failed_calls: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakdownBy {
    Repository,
    Provider,
    Key,
    Model,
}

#[derive(Debug, Serialize, FromRow)]
pub struct UsageBreakdown {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    pub label: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub calls: i64,
}

/// Rentang hari UTC `from..=to` sebagai kondisi `created_at` (memakai indeks `created_at`).
/// `$1`/`$2` = tanggal awal/akhir.
const IN_RANGE: &str = "u.created_at >= ($1::date)::timestamp AT TIME ZONE 'UTC' \
                        AND u.created_at < (($2::date + 1))::timestamp AT TIME ZONE 'UTC'";

/// Satu baris per hari dari `from` sampai `to` (inklusif), hari kosong diisi nol.
pub async fn daily(pool: &PgPool, from: NaiveDate, to: NaiveDate) -> Result<Vec<DailyUsage>> {
    sqlx::query_as(
        "SELECT d::date AS date, \
                COALESCE(SUM(u.input_tokens), 0)::bigint AS input_tokens, \
                COALESCE(SUM(u.output_tokens), 0)::bigint AS output_tokens, \
                COALESCE(SUM(u.cache_read_tokens), 0)::bigint AS cache_read_tokens, \
                COALESCE(SUM(u.cache_write_tokens), 0)::bigint AS cache_write_tokens, \
                COUNT(u.id) AS calls, \
                COUNT(u.id) FILTER (WHERE NOT u.success) AS failed_calls \
         FROM generate_series($1::timestamp, $2::timestamp, interval '1 day') AS d \
         LEFT JOIN llm_usage u \
           ON u.created_at >= d AT TIME ZONE 'UTC' \
          AND u.created_at < (d + interval '1 day') AT TIME ZONE 'UTC' \
         GROUP BY d ORDER BY d",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
}

/// Total per repo/provider/key/model, token terbanyak dulu. Baris yang FK-nya sudah dihapus
/// (di-NULL-kan) dilabeli "(deleted)".
pub async fn breakdown(
    pool: &PgPool,
    by: BreakdownBy,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<UsageBreakdown>> {
    // Potongan SQL ini tetap (dari enum), bukan input pengguna.
    let (id, label, join) = match by {
        BreakdownBy::Repository => (
            "u.repository_id",
            format!(
                "COALESCE({}, '(deleted)')",
                repositories::display_name_sql("r")
            ),
            "LEFT JOIN repositories r ON r.id = u.repository_id",
        ),
        BreakdownBy::Provider => (
            "u.provider_id",
            "COALESCE(p.name, '(deleted)')".to_string(),
            "LEFT JOIN ai_providers p ON p.id = u.provider_id",
        ),
        BreakdownBy::Key => (
            "u.key_id",
            "COALESCE(k.label, '(deleted)')".to_string(),
            "LEFT JOIN ai_keys k ON k.id = u.key_id",
        ),
        BreakdownBy::Model => ("NULL::uuid", "u.model".to_string(), ""),
    };
    sqlx::query_as(&format!(
        "SELECT {id} AS id, {label} AS label, \
                COALESCE(SUM(u.input_tokens), 0)::bigint AS input_tokens, \
                COALESCE(SUM(u.output_tokens), 0)::bigint AS output_tokens, \
                COALESCE(SUM(u.cache_read_tokens), 0)::bigint AS cache_read_tokens, \
                COALESCE(SUM(u.cache_write_tokens), 0)::bigint AS cache_write_tokens, \
                COUNT(*) AS calls \
         FROM llm_usage u {join} WHERE {IN_RANGE} \
         GROUP BY 1, 2 \
         ORDER BY SUM(u.input_tokens + u.output_tokens) DESC, 2"
    ))
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
}
