use std::time::{Duration, Instant};

use adus_core::types::ProviderKind;
use adus_db::{
    ai::{self, AiKey, AiKeyStatus, AiProvider, KeyPatch},
    usage::{self, NewUsage},
};
use adus_llm::{ChatRequest, LlmError, client_for};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, patch, post},
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use super::{connections::required, repositories::present_or_null};
use crate::{
    auth::{AdminUser, CurrentUser},
    error::{ApiError, ApiJson, ApiPath},
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ai-providers", get(list).post(create))
        .route("/ai-providers/models", post(discover_models))
        .route("/ai-providers/{id}", patch(update).delete(remove))
        .route("/ai-providers/{id}/keys", post(create_key))
        .route("/ai-keys/{id}", patch(update_key).delete(remove_key))
        .route("/ai-keys/{id}/test", post(test_key))
}

const MODEL_LIST_CACHE_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Deserialize)]
struct DiscoverModels {
    kind: Option<ProviderKind>,
    base_url: Option<String>,
    key: Option<String>,
    provider_id: Option<Uuid>,
    #[serde(default)]
    refresh: bool,
}

#[derive(Serialize)]
struct DiscoverModelsReply {
    models: Vec<String>,
    cached: bool,
}

#[derive(Deserialize)]
struct CreateProvider {
    kind: ProviderKind,
    name: String,
    base_url: Option<String>,
    primary_model: String,
    fast_model: Option<String>,
    key: Option<String>,
    key_label: Option<String>,
}

#[derive(Deserialize)]
struct UpdateProvider {
    name: Option<String>,
    /// Ada dan `null` (atau teks kosong) = kosongkan; tidak ada = biarkan.
    #[serde(default, deserialize_with = "present_or_null")]
    base_url: Option<Option<String>>,
    primary_model: Option<String>,
    /// Ada dan `null` (atau teks kosong) = kosongkan; tidak ada = biarkan.
    #[serde(default, deserialize_with = "present_or_null")]
    fast_model: Option<Option<String>>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
struct CreateKey {
    label: String,
    key: String,
}

#[derive(Deserialize)]
struct UpdateKey {
    label: Option<String>,
    enabled: Option<bool>,
    key: Option<String>,
}

#[derive(Deserialize)]
struct TestKey {
    #[serde(default)]
    model: Option<String>,
}

#[derive(Serialize)]
struct ProviderJson {
    id: Uuid,
    kind: ProviderKind,
    name: String,
    base_url: Option<String>,
    /// `null` = the provider still needs a model (set up before models were per provider).
    primary_model: Option<String>,
    fast_model: Option<String>,
    enabled: bool,
    created_at: DateTime<Utc>,
    keys: Vec<KeyJson>,
}

/// Respons key. Tidak pernah berisi key maupun `encrypted_key`, hanya 4 karakter terakhir.
#[derive(Serialize)]
struct KeyJson {
    id: Uuid,
    label: String,
    key_last4: String,
    enabled: bool,
    status: AiKeyStatus,
    cooldown_until: Option<DateTime<Utc>>,
    last_used_at: Option<DateTime<Utc>>,
}

impl From<AiKey> for KeyJson {
    fn from(k: AiKey) -> KeyJson {
        KeyJson {
            id: k.id,
            label: k.label,
            key_last4: k.key_last4,
            enabled: k.enabled,
            status: k.status,
            cooldown_until: k.cooldown_until,
            last_used_at: k.last_used_at,
        }
    }
}

#[derive(Serialize)]
struct TestReply {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn provider_json(p: AiProvider, keys: Vec<AiKey>) -> ProviderJson {
    ProviderJson {
        id: p.id,
        kind: p.kind,
        name: p.name,
        base_url: p.base_url,
        primary_model: p.primary_model,
        fast_model: p.fast_model,
        enabled: p.enabled,
        created_at: p.created_at,
        keys: keys.into_iter().map(KeyJson::from).collect(),
    }
}

fn clean_optional_model(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

async fn provider_with_keys(state: &AppState, p: AiProvider) -> Result<ProviderJson, ApiError> {
    let keys = ai::list_keys(&state.db, p.id).await?;
    Ok(provider_json(p, keys))
}

/// Teks kosong = tidak diisi. Yang diisi harus URL http(s) dengan host.
fn clean_base_url(raw: Option<&str>) -> Result<Option<String>, ApiError> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let ok = reqwest::Url::parse(raw)
        .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some());
    if !ok {
        return Err(ApiError::validation("base_url must be an http(s) URL"));
    }
    Ok(Some(raw.to_string()))
}

fn check_compatible_has_url(kind: ProviderKind, base_url: &Option<String>) -> Result<(), ApiError> {
    if kind == ProviderKind::OpenaiCompatible && base_url.is_none() {
        return Err(ApiError::validation(
            "base_url is required for openai_compatible providers",
        ));
    }
    Ok(())
}

async fn find_provider(state: &AppState, id: Uuid) -> Result<AiProvider, ApiError> {
    ai::get_provider(&state.db, id)
        .await?
        .ok_or_else(ApiError::not_found)
}

async fn list(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<ProviderJson>>, ApiError> {
    let mut out = Vec::new();
    for p in ai::list_providers(&state.db).await? {
        out.push(provider_with_keys(&state, p).await?);
    }
    Ok(Json(out))
}

async fn create(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateProvider>,
) -> Result<(StatusCode, Json<ProviderJson>), ApiError> {
    let name = required(&body.name, "name")?;
    let primary_model = required(&body.primary_model, "primary_model")?;
    let fast_model = clean_optional_model(body.fast_model.as_deref());
    let base_url = clean_base_url(body.base_url.as_deref())?;
    check_compatible_has_url(body.kind, &base_url)?;
    let p = ai::insert_provider(
        &state.db,
        body.kind,
        &name,
        base_url.as_deref(),
        &primary_model,
        fast_model.as_deref(),
    )
    .await?;
    let mut keys = Vec::new();
    if let Some(secret) = body.key.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let label = body
            .key_label
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Default");
        let key = ai::insert_key(
            &state.db,
            p.id,
            label,
            &state.cipher.encrypt(secret),
            &adus_core::crypto::last4(secret),
        )
        .await?;
        keys.push(key);
    }
    Ok((StatusCode::CREATED, Json(provider_json(p, keys))))
}

fn provider_kind_key(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Openai => "openai",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Gemini => "gemini",
        ProviderKind::OpenaiCompatible => "openai_compatible",
    }
}

/// Redis key for a cached model list. Keyed by an HMAC under `ADUS_SECRET_KEY`, so the
/// key name never holds anything derived from the API key alone.
fn model_list_cache_key(
    secret_key: &str,
    kind: ProviderKind,
    base_url: Option<&str>,
    api_key: &str,
) -> Result<String, ApiError> {
    let norm_url = base_url.map(|u| u.trim_end_matches('/')).unwrap_or("");
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret_key.as_bytes()).map_err(|_| ApiError::internal())?;
    mac.update(provider_kind_key(kind).as_bytes());
    mac.update(b"\0");
    mac.update(norm_url.as_bytes());
    mac.update(b"\0");
    mac.update(api_key.as_bytes());
    Ok(format!(
        "models:v2:{}",
        hex::encode(mac.finalize().into_bytes())
    ))
}

fn same_base_url(a: Option<&str>, b: Option<&str>) -> bool {
    let norm = |u: Option<&str>| u.map(|u| u.trim().trim_end_matches('/').to_string());
    norm(a) == norm(b)
}

async fn discover_models(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<DiscoverModels>,
) -> Result<Json<DiscoverModelsReply>, ApiError> {
    let (kind, base_url, secret) = if let Some(provider_id) = body.provider_id {
        let provider = find_provider(&state, provider_id).await?;
        let base_url = match &body.base_url {
            Some(raw) => clean_base_url(Some(raw))?,
            None => provider.base_url.clone(),
        };
        check_compatible_has_url(provider.kind, &base_url)?;
        let inline_key = body.key.as_deref().map(str::trim).filter(|s| !s.is_empty());
        // A stored key only ever goes to the provider's saved URL. Testing a new URL needs the
        // key typed in again, so nobody can send a saved key somewhere else without saving.
        if inline_key.is_none() && !same_base_url(base_url.as_deref(), provider.base_url.as_deref())
        {
            return Err(ApiError::validation(
                "enter the API key to test a different base URL",
            ));
        }
        let secret = match inline_key {
            Some(raw) => raw.to_string(),
            None => {
                let keys = ai::list_keys(&state.db, provider_id).await?;
                let stored = keys
                    .iter()
                    .find(|k| k.enabled && k.status != AiKeyStatus::Invalid)
                    .or_else(|| keys.iter().find(|k| k.enabled))
                    .ok_or_else(|| {
                        ApiError::validation("an API key is required to test the connection")
                    })?;
                state.cipher.decrypt(&stored.encrypted_key).map_err(|_| {
                    ApiError::new(
                        StatusCode::BAD_GATEWAY,
                        "provider_error",
                        "the stored key could not be decrypted",
                    )
                })?
            }
        };
        (provider.kind, base_url, secret)
    } else {
        let kind = body
            .kind
            .ok_or_else(|| ApiError::validation("kind is required"))?;
        let base_url = clean_base_url(body.base_url.as_deref())?;
        check_compatible_has_url(kind, &base_url)?;
        let secret = required(body.key.as_deref().unwrap_or(""), "key")?;
        (kind, base_url, secret)
    };

    let cache_key =
        model_list_cache_key(&state.config.secret_key, kind, base_url.as_deref(), &secret)?;
    if !body.refresh {
        match state.queue.cache_get(&cache_key).await {
            Ok(Some(raw)) => match serde_json::from_str::<Vec<String>>(&raw) {
                Ok(models) => {
                    return Ok(Json(DiscoverModelsReply {
                        models,
                        cached: true,
                    }));
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed to decode cached model list");
                }
            },
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(error = %err, "failed to read cached model list from Redis");
            }
        }
    }

    let models = match client_for(kind, base_url.as_deref(), state.http.clone())
        .list_models(&secret)
        .await
    {
        Ok(models) => models,
        Err(LlmError::Unauthorized) => {
            if let Err(err) = state.queue.cache_del(&cache_key).await {
                tracing::warn!(error = %err, "failed to evict revoked key model cache");
            }
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "token_invalid",
                "the provider rejected the API key",
            ));
        }
        Err(other) => {
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "provider_error",
                other.to_string(),
            ));
        }
    };

    if !models.is_empty() {
        match serde_json::to_string(&models) {
            Ok(raw) => {
                if let Err(err) = state
                    .queue
                    .cache_set(&cache_key, &raw, MODEL_LIST_CACHE_TTL)
                    .await
                {
                    tracing::warn!(error = %err, "failed to write cached model list to Redis");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to serialize model list for cache");
            }
        }
    }

    Ok(Json(DiscoverModelsReply {
        models,
        cached: false,
    }))
}

async fn update(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<UpdateProvider>,
) -> Result<Json<ProviderJson>, ApiError> {
    let current = find_provider(&state, id).await?;
    let name = match &body.name {
        Some(n) => required(n, "name")?,
        None => current.name,
    };
    let base_url = match &body.base_url {
        Some(new) => clean_base_url(new.as_deref())?,
        None => current.base_url,
    };
    check_compatible_has_url(current.kind, &base_url)?;
    let primary_model = match &body.primary_model {
        Some(m) => Some(required(m, "primary_model")?),
        None => current.primary_model,
    };
    let fast_model = match &body.fast_model {
        Some(new) => clean_optional_model(new.as_deref()),
        None => current.fast_model,
    };
    let enabled = body.enabled.unwrap_or(current.enabled);
    let p = ai::update_provider(
        &state.db,
        id,
        &name,
        base_url.as_deref(),
        primary_model.as_deref(),
        fast_model.as_deref(),
        enabled,
    )
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(provider_with_keys(&state, p).await?))
}

async fn remove(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    if ai::provider_in_use(&state.db, id).await? {
        return Err(ApiError::conflict(
            "provider_in_use",
            "the provider is the default in settings or used by a repository",
        ));
    }
    if ai::delete_provider(&state.db, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}

async fn create_key(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiPath(provider_id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<CreateKey>,
) -> Result<(StatusCode, Json<KeyJson>), ApiError> {
    let label = required(&body.label, "label")?;
    let secret = required(&body.key, "key")?;
    find_provider(&state, provider_id).await?;
    let key = ai::insert_key(
        &state.db,
        provider_id,
        &label,
        &state.cipher.encrypt(&secret),
        &adus_core::crypto::last4(&secret),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(key.into())))
}

async fn update_key(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<UpdateKey>,
) -> Result<Json<KeyJson>, ApiError> {
    let label = body
        .label
        .as_deref()
        .map(|l| required(l, "label"))
        .transpose()?;
    let secret = body
        .key
        .as_deref()
        .map(|k| required(k, "key"))
        .transpose()?;
    // Key baru: status kembali `ok` dan cooldown dihapus (dikerjakan oleh `update_key`).
    let patch = KeyPatch {
        label,
        enabled: body.enabled,
        secret: secret.map(|s| (state.cipher.encrypt(&s), adus_core::crypto::last4(&s))),
    };
    let key = ai::update_key(&state.db, id, patch)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(key.into()))
}

async fn remove_key(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    if ai::delete_key(&state.db, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}

/// Nama `error_kind` yang sama dengan yang dipakai router.
fn error_kind(err: &LlmError) -> &'static str {
    match err {
        LlmError::RateLimited { .. } => "rate_limited",
        LlmError::Unauthorized => "unauthorized",
        LlmError::Transient(_) => "transient",
        LlmError::Invalid(_) => "invalid",
        LlmError::ToolsUnsupported(_) => "tools_unsupported",
        LlmError::ContextOverflow(_) => "context_overflow",
    }
}

const TEST_KEY_MAX_TOKENS: u32 = 512;

/// Satu panggilan "ping" ke provider dengan key ini. Tidak lewat router dan tidak pernah
/// mengubah status key; hasilnya hanya dilaporkan dan dicatat sebagai usage.
async fn test_key(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<TestKey>,
) -> Result<Json<TestReply>, ApiError> {
    let key = ai::get_key(&state.db, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let provider = find_provider(&state, key.provider_id).await?;
    let model = body
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .or_else(|| provider.effective_fast_model())
        .ok_or_else(|| {
            ApiError::validation("the provider has no model yet: choose a primary model first")
        })?
        .to_string();

    let started = Instant::now();
    let outcome = match state.cipher.decrypt(&key.encrypted_key) {
        Err(_) => Err((
            "decrypt",
            "the stored key could not be decrypted".to_string(),
        )),
        Ok(secret) => {
            let req = ChatRequest::text(model.as_str(), "", "ping", TEST_KEY_MAX_TOKENS);
            client_for(
                provider.kind,
                provider.base_url.as_deref(),
                state.http.clone(),
            )
            .chat(&secret, &req)
            .await
            .map_err(|e| (error_kind(&e), e.to_string()))
        }
    };
    let latency_ms = i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX);

    let tokens = |n: u32| i32::try_from(n).unwrap_or(i32::MAX);
    let row = NewUsage {
        review_id: None,
        repository_id: None,
        provider_id: provider.id,
        key_id: key.id,
        model: &model,
        input_tokens: outcome.as_ref().map_or(0, |r| tokens(r.input_tokens)),
        output_tokens: outcome.as_ref().map_or(0, |r| tokens(r.output_tokens)),
        cache_read_tokens: outcome.as_ref().map_or(0, |r| tokens(r.cache_read_tokens)),
        cache_write_tokens: outcome.as_ref().map_or(0, |r| tokens(r.cache_write_tokens)),
        latency_ms,
        success: outcome.is_ok(),
        error_kind: outcome.as_ref().err().map(|(kind, _)| *kind),
    };
    // Gagal mencatat tidak boleh menggagalkan hasil tes.
    if let Err(err) = usage::insert(&state.db, row).await {
        tracing::warn!("failed to write usage row for key test: {err}");
    }

    Ok(Json(match outcome {
        Ok(_) => TestReply {
            ok: true,
            error: None,
        },
        Err((_, message)) => TestReply {
            ok: false,
            error: Some(message),
        },
    }))
}
