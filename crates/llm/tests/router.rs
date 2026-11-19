use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adus_core::crypto::{Cipher, last4};
use adus_core::types::ProviderKind;
use adus_db::ai::{self, AiKey, AiKeyStatus, AiProvider};
use adus_llm::router::{ChatError, ChatService, LlmRouter, UsageContext};
use adus_llm::{ChatRequest, ChatResponse, LlmClient, LlmError};
use async_trait::async_trait;
use chrono::Utc;
use sqlx::PgPool;

const SECRET_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

/// Client palsu: tiap API key punya antrian hasil. Antrian kosong -> sukses.
/// `calls` mencatat API key (plaintext) yang dipakai, berurutan.
#[derive(Default)]
struct Fake {
    scripts: Mutex<HashMap<String, VecDeque<Result<ChatResponse, LlmError>>>>,
    calls: Mutex<Vec<String>>,
}

impl Fake {
    fn script(&self, api_key: &str, results: Vec<Result<ChatResponse, LlmError>>) {
        self.scripts
            .lock()
            .unwrap()
            .insert(api_key.to_string(), results.into());
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

/// `LlmRouter` meminta `Box<dyn LlmClient>`; ini meneruskan ke `Fake` yang dibagi dengan test.
struct SharedFake(Arc<Fake>);

#[async_trait]
impl LlmClient for SharedFake {
    async fn chat(&self, api_key: &str, _req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.0.calls.lock().unwrap().push(api_key.to_string());
        let next = self
            .0
            .scripts
            .lock()
            .unwrap()
            .get_mut(api_key)
            .and_then(|queue| queue.pop_front());
        next.unwrap_or_else(|| Ok(ok_response()))
    }
}

fn ok_response() -> ChatResponse {
    ChatResponse {
        text: "hello".into(),
        input_tokens: 10,
        output_tokens: 5,
    }
}

fn rate_limited(secs: u64) -> Result<ChatResponse, LlmError> {
    Err(LlmError::RateLimited {
        retry_after: Some(Duration::from_secs(secs)),
    })
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-test".into(),
        system: "sys".into(),
        user: "user".into(),
        max_tokens: 100,
    }
}

struct Setup {
    router: LlmRouter,
    fake: Arc<Fake>,
    provider: AiProvider,
    /// Key ke-i memakai API key plaintext `key-i`.
    keys: Vec<AiKey>,
}

async fn setup(pool: &PgPool, kind: ProviderKind, key_count: usize) -> Setup {
    let cipher = Arc::new(Cipher::from_base64_key(SECRET_KEY).unwrap());
    let provider = ai::insert_provider(pool, kind, "test", Some("http://fake.local"))
        .await
        .unwrap();
    let mut keys = Vec::new();
    for i in 0..key_count {
        let plain = format!("key-{i}");
        let key = ai::insert_key(
            pool,
            provider.id,
            &format!("label {i}"),
            &cipher.encrypt(&plain),
            &last4(&plain),
        )
        .await
        .unwrap();
        keys.push(key);
    }
    let fake = Arc::new(Fake::default());
    let for_factory = fake.clone();
    let router = LlmRouter::with_client_factory(
        pool.clone(),
        cipher,
        Box::new(move |_kind, _base_url| Box::new(SharedFake(for_factory.clone()))),
    );
    Setup {
        router,
        fake,
        provider,
        keys,
    }
}

async fn chat(s: &Setup) -> Result<ChatResponse, ChatError> {
    s.router
        .chat(s.provider.id, request(), UsageContext::default())
        .await
}

async fn key_state(pool: &PgPool, s: &Setup, index: usize) -> AiKey {
    ai::list_keys(pool, s.provider.id)
        .await
        .unwrap()
        .into_iter()
        .find(|k| k.id == s.keys[index].id)
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn round_robin_uses_least_recently_used_key(pool: PgPool) {
    let s = setup(&pool, ProviderKind::Openai, 2).await;
    for _ in 0..4 {
        chat(&s).await.unwrap();
    }
    let calls = s.fake.calls();
    assert_ne!(calls[0], calls[1]);
    assert_eq!(calls[0], calls[2]);
    assert_eq!(calls[1], calls[3]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn rate_limited_key_cooled_down_and_next_key_used(pool: PgPool) {
    let s = setup(&pool, ProviderKind::Openai, 2).await;
    // key-1 baru dipakai, jadi key-0 yang dicoba lebih dulu.
    ai::mark_used(&pool, s.keys[1].id, Utc::now())
        .await
        .unwrap();
    s.fake.script("key-0", vec![rate_limited(30)]);

    let before = Utc::now();
    let response = chat(&s).await.unwrap();
    assert_eq!(response, ok_response());
    assert_eq!(s.fake.calls(), ["key-0", "key-1"]);

    let cooled = key_state(&pool, &s, 0).await;
    assert_eq!(cooled.status, AiKeyStatus::RateLimited);
    let until = cooled.cooldown_until.unwrap();
    assert!(until >= before + chrono::Duration::seconds(30));
    assert!(until <= Utc::now() + chrono::Duration::seconds(30));

    // Selama cooldown key-0 dilewati.
    chat(&s).await.unwrap();
    assert_eq!(s.fake.calls(), ["key-0", "key-1", "key-1"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn rate_limit_without_retry_after_defaults_to_60_seconds(pool: PgPool) {
    let s = setup(&pool, ProviderKind::Openai, 1).await;
    s.fake.script(
        "key-0",
        vec![Err(LlmError::RateLimited { retry_after: None })],
    );
    let before = Utc::now();
    let err = chat(&s).await.unwrap_err();
    assert_eq!(
        err,
        ChatError::AllKeysFailed {
            retry_after: Some(Duration::from_secs(60))
        }
    );
    let until = key_state(&pool, &s, 0).await.cooldown_until.unwrap();
    assert!(until >= before + chrono::Duration::seconds(60));
}

#[sqlx::test(migrations = "../../migrations")]
async fn cooled_down_key_returns_after_until(pool: PgPool) {
    let s = setup(&pool, ProviderKind::Openai, 1).await;
    let now = Utc::now();
    ai::mark_cooldown(&pool, s.keys[0].id, now + chrono::Duration::seconds(60))
        .await
        .unwrap();
    // Sebelum `until`: tidak ada key. Sesudahnya: key kembali.
    let early = ai::candidate_keys(&pool, s.provider.id, now).await.unwrap();
    assert!(early.is_empty());
    let late = ai::candidate_keys(&pool, s.provider.id, now + chrono::Duration::seconds(61))
        .await
        .unwrap();
    assert_eq!(late.len(), 1);

    // Cooldown yang sudah lewat: router memakai key lagi dan memulihkannya ke `ok`.
    ai::mark_cooldown(&pool, s.keys[0].id, now - chrono::Duration::seconds(1))
        .await
        .unwrap();
    chat(&s).await.unwrap();
    let key = key_state(&pool, &s, 0).await;
    assert_eq!(key.status, AiKeyStatus::Ok);
    assert!(key.cooldown_until.is_none());
    assert!(key.last_used_at.is_some());
}

#[sqlx::test(migrations = "../../migrations")]
async fn unauthorized_key_marked_invalid_and_skipped_next_time(pool: PgPool) {
    let s = setup(&pool, ProviderKind::Openai, 2).await;
    ai::mark_used(&pool, s.keys[1].id, Utc::now())
        .await
        .unwrap();
    s.fake.script("key-0", vec![Err(LlmError::Unauthorized)]);

    chat(&s).await.unwrap();
    assert_eq!(s.fake.calls(), ["key-0", "key-1"]);
    assert_eq!(key_state(&pool, &s, 0).await.status, AiKeyStatus::Invalid);

    chat(&s).await.unwrap();
    assert_eq!(s.fake.calls(), ["key-0", "key-1", "key-1"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn all_keys_rate_limited_returns_all_keys_failed_with_retry_after(pool: PgPool) {
    let s = setup(&pool, ProviderKind::Openai, 2).await;
    s.fake.script("key-0", vec![rate_limited(90)]);
    s.fake.script("key-1", vec![rate_limited(30)]);

    let err = chat(&s).await.unwrap_err();
    assert_eq!(
        err,
        ChatError::AllKeysFailed {
            retry_after: Some(Duration::from_secs(30))
        }
    );
    // Setiap key dicoba tepat sekali.
    assert_eq!(s.fake.calls().len(), 2);
    // Semua key sedang cooldown.
    assert_eq!(chat(&s).await.unwrap_err(), ChatError::NoKeys);
}

#[sqlx::test(migrations = "../../migrations")]
async fn all_transient_failures_have_no_retry_after_and_keys_stay_usable(pool: PgPool) {
    let s = setup(&pool, ProviderKind::Openai, 2).await;
    s.fake
        .script("key-0", vec![Err(LlmError::Transient("boom".into()))]);
    s.fake
        .script("key-1", vec![Err(LlmError::Transient("boom".into()))]);

    let err = chat(&s).await.unwrap_err();
    assert_eq!(err, ChatError::AllKeysFailed { retry_after: None });
    assert_eq!(
        ai::candidate_keys(&pool, s.provider.id, Utc::now())
            .await
            .unwrap()
            .len(),
        2
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn invalid_request_stops_without_trying_other_keys(pool: PgPool) {
    let s = setup(&pool, ProviderKind::Openai, 2).await;
    s.fake
        .script("key-0", vec![Err(LlmError::Invalid("bad model".into()))]);
    s.fake
        .script("key-1", vec![Err(LlmError::Invalid("bad model".into()))]);

    let err = chat(&s).await.unwrap_err();
    assert!(matches!(err, ChatError::Invalid(m) if m.contains("bad model")));
    assert_eq!(s.fake.calls().len(), 1);
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM llm_usage WHERE error_kind = 'invalid'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn undecryptable_key_marked_invalid_and_next_key_used(pool: PgPool) {
    let s = setup(&pool, ProviderKind::Openai, 2).await;
    sqlx::query("UPDATE ai_keys SET encrypted_key = 'not-valid-ciphertext' WHERE id = $1")
        .bind(s.keys[0].id)
        .execute(&pool)
        .await
        .unwrap();
    ai::mark_used(&pool, s.keys[1].id, Utc::now())
        .await
        .unwrap();

    chat(&s).await.unwrap();
    // key-0 tidak pernah sampai ke client.
    assert_eq!(s.fake.calls(), ["key-1"]);
    assert_eq!(key_state(&pool, &s, 0).await.status, AiKeyStatus::Invalid);
    let kinds: Vec<Option<String>> =
        sqlx::query_scalar("SELECT error_kind FROM llm_usage WHERE success = false")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(kinds, [Some("decrypt".to_string())]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn every_attempt_writes_usage_row(pool: PgPool) {
    let s = setup(&pool, ProviderKind::Openai, 2).await;
    ai::mark_used(&pool, s.keys[1].id, Utc::now())
        .await
        .unwrap();
    s.fake.script("key-0", vec![rate_limited(30)]);

    chat(&s).await.unwrap();

    #[derive(sqlx::FromRow)]
    struct Row {
        key_id: uuid::Uuid,
        provider_id: uuid::Uuid,
        model: String,
        input_tokens: i32,
        output_tokens: i32,
        latency_ms: i32,
        success: bool,
        error_kind: Option<String>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT key_id, provider_id, model, input_tokens, output_tokens, latency_ms, success, \
         error_kind FROM llm_usage ORDER BY created_at, id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);

    assert_eq!(rows[0].key_id, s.keys[0].id);
    assert!(!rows[0].success);
    assert_eq!(rows[0].error_kind.as_deref(), Some("rate_limited"));
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (0, 0));

    assert_eq!(rows[1].key_id, s.keys[1].id);
    assert!(rows[1].success);
    assert_eq!(rows[1].error_kind, None);
    assert_eq!((rows[1].input_tokens, rows[1].output_tokens), (10, 5));

    for row in &rows {
        assert_eq!(row.provider_id, s.provider.id);
        assert_eq!(row.model, "gpt-test");
        assert!(row.latency_ms >= 0);
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn usage_row_carries_review_and_repository_context(pool: PgPool) {
    let s = setup(&pool, ProviderKind::Openai, 1).await;
    let review_id = uuid::Uuid::new_v4();
    let ctx = UsageContext {
        review_id: Some(review_id),
        repository_id: None,
    };
    s.router.chat(s.provider.id, request(), ctx).await.unwrap();
    let got: Option<uuid::Uuid> = sqlx::query_scalar("SELECT review_id FROM llm_usage")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(got, Some(review_id));
}

#[sqlx::test(migrations = "../../migrations")]
async fn disabled_key_never_used(pool: PgPool) {
    let s = setup(&pool, ProviderKind::Openai, 2).await;
    let patch = ai::KeyPatch {
        enabled: Some(false),
        ..Default::default()
    };
    ai::update_key(&pool, s.keys[0].id, patch).await.unwrap();

    for _ in 0..3 {
        chat(&s).await.unwrap();
    }
    assert_eq!(s.fake.calls(), ["key-1", "key-1", "key-1"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn no_keys_and_disabled_provider_return_no_keys(pool: PgPool) {
    let empty = setup(&pool, ProviderKind::Openai, 0).await;
    assert_eq!(chat(&empty).await.unwrap_err(), ChatError::NoKeys);

    let s = setup(&pool, ProviderKind::Openai, 1).await;
    ai::update_provider(
        &pool,
        s.provider.id,
        "test",
        Some("http://fake.local"),
        false,
    )
    .await
    .unwrap();
    assert_eq!(chat(&s).await.unwrap_err(), ChatError::NoKeys);
    assert!(s.fake.calls().is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn provider_accepts_many_compatible_keys(pool: PgPool) {
    let s = setup(&pool, ProviderKind::OpenaiCompatible, 5).await;
    let stored = ai::list_keys(&pool, s.provider.id).await.unwrap();
    assert_eq!(stored.len(), 5);
    assert_eq!(s.provider.kind, ProviderKind::OpenaiCompatible);
    let candidates = ai::candidate_keys(&pool, s.provider.id, Utc::now())
        .await
        .unwrap();
    assert_eq!(candidates.len(), 5);
    // Dan semuanya ikut dirotasi.
    for _ in 0..5 {
        chat(&s).await.unwrap();
    }
    let mut used = s.fake.calls();
    used.sort();
    used.dedup();
    assert_eq!(used.len(), 5);
}

#[sqlx::test(migrations = "../../migrations")]
async fn replacing_key_secret_revives_invalid_key(pool: PgPool) {
    let s = setup(&pool, ProviderKind::Openai, 1).await;
    ai::mark_invalid(&pool, s.keys[0].id).await.unwrap();
    assert_eq!(chat(&s).await.unwrap_err(), ChatError::NoKeys);

    let patch = ai::KeyPatch {
        secret: Some(("whatever".into(), "7890".into())),
        ..Default::default()
    };
    let key = ai::update_key(&pool, s.keys[0].id, patch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(key.status, AiKeyStatus::Ok);
    assert_eq!(key.key_last4, "7890");
    assert_eq!(key.label, "label 0");
}

#[sqlx::test(migrations = "../../migrations")]
async fn deleting_key_and_provider_keeps_usage_history(pool: PgPool) {
    let s = setup(&pool, ProviderKind::Openai, 1).await;
    chat(&s).await.unwrap();
    assert!(ai::delete_key(&pool, s.keys[0].id).await.unwrap());
    assert!(ai::delete_provider(&pool, s.provider.id).await.unwrap());
    let (key_id, provider_id): (Option<uuid::Uuid>, Option<uuid::Uuid>) =
        sqlx::query_as("SELECT key_id, provider_id FROM llm_usage")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((key_id, provider_id), (None, None));
}

#[sqlx::test(migrations = "../../migrations")]
async fn settings_row_exists_with_defaults(pool: PgPool) {
    let current = adus_db::settings::get(&pool).await.unwrap();
    assert_eq!(current.max_files_per_review, 50);
    assert_eq!(current.max_patch_lines_per_file, 2000);
    assert_eq!(current.default_provider_id, None);

    let provider = ai::insert_provider(&pool, ProviderKind::Gemini, "g", None)
        .await
        .unwrap();
    let new = adus_db::settings::Settings {
        default_provider_id: Some(provider.id),
        default_model: Some("gemini-x".into()),
        max_files_per_review: 10,
        max_patch_lines_per_file: 500,
    };
    assert_eq!(adus_db::settings::update(&pool, &new).await.unwrap(), new);
    // Hapus provider -> default_provider_id di-NULL-kan.
    ai::delete_provider(&pool, provider.id).await.unwrap();
    assert_eq!(
        adus_db::settings::get(&pool)
            .await
            .unwrap()
            .default_provider_id,
        None
    );
}
