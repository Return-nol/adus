use adus_core::types::ProviderKind;
use adus_db::ai;
use adus_db::usage::{self, BreakdownBy, NewUsage};
use chrono::NaiveDate;
use sqlx::PgPool;
use uuid::Uuid;

/// One provider with one key; returns `(provider_id, key_id)`.
async fn provider_and_key(pool: &PgPool) -> (Uuid, Uuid) {
    let provider = ai::insert_provider(pool, ProviderKind::Anthropic, "p", None, "m", None)
        .await
        .unwrap();
    let key = ai::insert_key(pool, provider.id, "main", "encrypted", "1234")
        .await
        .unwrap();
    (provider.id, key.id)
}

fn new_usage(
    provider_id: Uuid,
    key_id: Uuid,
    input: i32,
    read: i32,
    write: i32,
) -> NewUsage<'static> {
    NewUsage {
        review_id: None,
        repository_id: None,
        provider_id,
        key_id,
        model: "m",
        input_tokens: input,
        output_tokens: 1,
        cache_read_tokens: read,
        cache_write_tokens: write,
        latency_ms: 5,
        success: true,
        error_kind: None,
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn daily_and_breakdown_sum_cache_tokens(pool: PgPool) {
    let (provider_id, key_id) = provider_and_key(&pool).await;
    usage::insert(&pool, new_usage(provider_id, key_id, 100, 60, 30))
        .await
        .unwrap();
    usage::insert(&pool, new_usage(provider_id, key_id, 50, 40, 0))
        .await
        .unwrap();
    // Pin both rows to one known day.
    sqlx::query("UPDATE llm_usage SET created_at = '2026-03-02T12:00:00Z'")
        .execute(&pool)
        .await
        .unwrap();
    let day = |d: u32| NaiveDate::from_ymd_opt(2026, 3, d).unwrap();

    let daily = usage::daily(&pool, day(1), day(2)).await.unwrap();
    assert_eq!(daily.len(), 2);
    // A day without calls is zero-filled for the cache columns too.
    assert_eq!(
        (daily[0].cache_read_tokens, daily[0].cache_write_tokens),
        (0, 0)
    );
    assert_eq!(daily[1].input_tokens, 150);
    assert_eq!(
        (daily[1].cache_read_tokens, daily[1].cache_write_tokens),
        (100, 30)
    );

    let by_model = usage::breakdown(&pool, BreakdownBy::Model, day(1), day(2))
        .await
        .unwrap();
    assert_eq!(by_model.len(), 1);
    assert_eq!(
        (
            by_model[0].cache_read_tokens,
            by_model[0].cache_write_tokens
        ),
        (100, 30)
    );
}
