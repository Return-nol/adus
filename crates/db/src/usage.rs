use sqlx::PgPool;
use uuid::Uuid;

use crate::Result;

/// Satu baris per percobaan panggilan LLM (sukses maupun gagal).
pub struct NewUsage<'a> {
    pub review_id: Option<Uuid>,
    pub repository_id: Option<Uuid>,
    pub provider_id: Uuid,
    pub key_id: Uuid,
    pub model: &'a str,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub latency_ms: i32,
    pub success: bool,
    pub error_kind: Option<&'a str>,
}

pub async fn insert(pool: &PgPool, new: NewUsage<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO llm_usage \
         (review_id, repository_id, provider_id, key_id, model, input_tokens, output_tokens, \
          latency_ms, success, error_kind) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(new.review_id)
    .bind(new.repository_id)
    .bind(new.provider_id)
    .bind(new.key_id)
    .bind(new.model)
    .bind(new.input_tokens)
    .bind(new.output_tokens)
    .bind(new.latency_ms)
    .bind(new.success)
    .bind(new.error_kind)
    .execute(pool)
    .await?;
    Ok(())
}
