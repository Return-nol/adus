//! Router multi-key: satu provider punya banyak API key; key dirotasi, key yang kena 429
//! didinginkan, key yang ditolak (401/403) ditandai `invalid`, dan setiap percobaan dicatat
//! di `llm_usage`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use adus_core::crypto::Cipher;
use adus_core::types::ProviderKind;
use adus_db::ai::{self, AiKey, AiKeyStatus};
use adus_db::usage::{self, NewUsage};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{ChatRequest, ChatResponse, LlmClient, LlmError, client_for};

/// Cooldown bila provider tidak menyebut `Retry-After`.
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);

/// Untuk apa panggilan ini; ikut disimpan di baris usage.
#[derive(Debug, Clone, Default)]
pub struct UsageContext {
    pub review_id: Option<Uuid>,
    pub repository_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ChatError {
    /// Provider tidak punya key yang bisa dipakai: tidak ada key, semua disabled/invalid, atau
    /// provider dinonaktifkan/tidak ada. Key yang hanya cooldown menghasilkan `AllKeysFailed`.
    #[error("no usable API keys")]
    NoKeys,
    /// Semua key yang dicoba gagal. `retry_after` = cooldown terdekat di antara key yang kena
    /// rate limit pada panggilan ini; `None` jika tidak ada yang kena rate limit.
    #[error("all API keys failed")]
    AllKeysFailed { retry_after: Option<Duration> },
    /// Request-nya sendiri ditolak provider; key lain tidak akan membantu.
    #[error("invalid request: {0}")]
    Invalid(String),
    /// Database tidak bisa dibaca; panggilan LLM belum dicoba.
    #[error("database error: {0}")]
    Internal(String),
}

#[async_trait]
pub trait ChatService: Send + Sync {
    async fn chat(
        &self,
        provider_id: Uuid,
        req: ChatRequest,
        ctx: UsageContext,
    ) -> Result<ChatResponse, ChatError>;
}

type MakeClient = Box<dyn Fn(ProviderKind, Option<&str>) -> Box<dyn LlmClient> + Send + Sync>;

pub struct LlmRouter {
    db: PgPool,
    cipher: Arc<Cipher>,
    make_client: MakeClient,
}

impl LlmRouter {
    /// Router produksi: client HTTP asli per jenis provider.
    pub fn new(db: PgPool, cipher: Arc<Cipher>, http: reqwest::Client) -> LlmRouter {
        LlmRouter::with_client_factory(
            db,
            cipher,
            Box::new(move |kind, base_url| client_for(kind, base_url, http.clone())),
        )
    }

    /// Dipakai test untuk menyuntik client palsu.
    pub fn with_client_factory(
        db: PgPool,
        cipher: Arc<Cipher>,
        make_client: MakeClient,
    ) -> LlmRouter {
        LlmRouter {
            db,
            cipher,
            make_client,
        }
    }

    /// Provider aktif tapi tanpa key siap pakai. Ada key yang hanya sedang cooldown -> pemanggil
    /// bisa mencoba lagi nanti (`AllKeysFailed` dengan sisa cooldown terdekat); selain itu
    /// provider memang belum punya key yang bisa dipakai (`NoKeys`).
    async fn why_no_candidates(&self, provider_id: Uuid, now: DateTime<Utc>) -> ChatError {
        let keys = match ai::list_keys(&self.db, provider_id).await {
            Ok(keys) => keys,
            Err(e) => return ChatError::Internal(e.to_string()),
        };
        let soonest = keys
            .iter()
            .filter(|k| k.enabled && k.status != AiKeyStatus::Invalid)
            .filter_map(|k| k.cooldown_until)
            .filter(|until| *until > now)
            .min();
        match soonest {
            Some(until) => ChatError::AllKeysFailed {
                retry_after: Some((until - now).to_std().unwrap_or(Duration::ZERO)),
            },
            None => ChatError::NoKeys,
        }
    }

    /// Catat satu percobaan. Gagal mencatat tidak boleh menggagalkan panggilan LLM
    /// (yang mungkin sudah berhasil dan berbayar), jadi hanya di-log.
    async fn log_usage(
        &self,
        ctx: &UsageContext,
        key: &AiKey,
        req: &ChatRequest,
        latency_ms: i32,
        response: Option<&ChatResponse>,
        error_kind: Option<&str>,
    ) {
        let tokens = |n: u32| i32::try_from(n).unwrap_or(i32::MAX);
        let new = NewUsage {
            review_id: ctx.review_id,
            repository_id: ctx.repository_id,
            provider_id: key.provider_id,
            key_id: key.id,
            model: &req.model,
            input_tokens: response.map_or(0, |r| tokens(r.input_tokens)),
            output_tokens: response.map_or(0, |r| tokens(r.output_tokens)),
            latency_ms,
            success: error_kind.is_none(),
            error_kind,
        };
        warn_on_error("write usage row", usage::insert(&self.db, new).await);
    }
}

#[async_trait]
impl ChatService for LlmRouter {
    async fn chat(
        &self,
        provider_id: Uuid,
        req: ChatRequest,
        ctx: UsageContext,
    ) -> Result<ChatResponse, ChatError> {
        let now = Utc::now();
        let keys = ai::candidate_keys(&self.db, provider_id, now)
            .await
            .map_err(|e| ChatError::Internal(e.to_string()))?;
        let provider = ai::get_provider(&self.db, provider_id)
            .await
            .map_err(|e| ChatError::Internal(e.to_string()))?
            .filter(|p| p.enabled)
            .ok_or(ChatError::NoKeys)?;
        if keys.is_empty() {
            return Err(self.why_no_candidates(provider_id, now).await);
        }
        let client = (self.make_client)(provider.kind, provider.base_url.as_deref());

        // Cooldown terpendek di antara key yang kena rate limit pada panggilan ini.
        let mut soonest: Option<Duration> = None;
        // Tiap key dicoba maksimal sekali.
        for key in keys {
            let Ok(api_key) = self.cipher.decrypt(&key.encrypted_key) else {
                // Salah `ADUS_SECRET_KEY` atau data rusak: key ini tidak akan pernah bisa dipakai.
                warn_on_error("mark key invalid", ai::mark_invalid(&self.db, key.id).await);
                self.log_usage(&ctx, &key, &req, 0, None, Some("decrypt"))
                    .await;
                continue;
            };

            let started = Instant::now();
            let result = client.chat(&api_key, &req).await;
            let latency_ms = i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX);

            match result {
                Ok(response) => {
                    warn_on_error(
                        "mark key used",
                        ai::mark_used(&self.db, key.id, Utc::now()).await,
                    );
                    self.log_usage(&ctx, &key, &req, latency_ms, Some(&response), None)
                        .await;
                    return Ok(response);
                }
                Err(LlmError::RateLimited { retry_after }) => {
                    let cooldown = retry_after.unwrap_or(DEFAULT_COOLDOWN);
                    let wait = chrono::Duration::from_std(cooldown)
                        .unwrap_or(chrono::Duration::seconds(60));
                    warn_on_error(
                        "start key cooldown",
                        ai::mark_cooldown(&self.db, key.id, Utc::now() + wait).await,
                    );
                    soonest = Some(soonest.map_or(cooldown, |s| s.min(cooldown)));
                    self.log_usage(&ctx, &key, &req, latency_ms, None, Some("rate_limited"))
                        .await;
                }
                Err(LlmError::Unauthorized) => {
                    warn_on_error("mark key invalid", ai::mark_invalid(&self.db, key.id).await);
                    self.log_usage(&ctx, &key, &req, latency_ms, None, Some("unauthorized"))
                        .await;
                }
                Err(LlmError::Transient(_)) => {
                    self.log_usage(&ctx, &key, &req, latency_ms, None, Some("transient"))
                        .await;
                }
                Err(LlmError::Invalid(message)) => {
                    self.log_usage(&ctx, &key, &req, latency_ms, None, Some("invalid"))
                        .await;
                    return Err(ChatError::Invalid(message));
                }
            }
        }
        Err(ChatError::AllKeysFailed {
            retry_after: soonest,
        })
    }
}

/// Pembukuan key (status, usage) bersifat best-effort: kegagalannya hanya di-log.
fn warn_on_error(what: &str, result: adus_db::Result<()>) {
    if let Err(e) = result {
        tracing::warn!("failed to {what}: {e}");
    }
}
