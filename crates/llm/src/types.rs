//! Tipe bersama semua client LLM, plus satu pintu kirim request (`send`).

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::HeaderValue;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde_json::Value;

/// Timeout satu request ke provider. Dipasang per request supaya `reqwest::Client` bisa dibagi.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Batas panjang pesan error provider yang kita teruskan.
const MAX_PROVIDER_MESSAGE: usize = 200;
/// Batas atas `Retry-After`; mencegah overflow `Instant + Duration` di pemanggil.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub user: String,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatResponse {
    pub text: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LlmError {
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("unauthorized")]
    Unauthorized,
    #[error("transient error: {0}")]
    Transient(String),
    #[error("invalid request: {0}")]
    Invalid(String),
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn chat(&self, api_key: &str, req: &ChatRequest) -> Result<ChatResponse, LlmError>;
}

/// Kirim request lalu petakan status gagal ke `LlmError`.
/// API key, URL, dan body request tidak pernah masuk pesan error.
pub(crate) async fn send(req: RequestBuilder) -> Result<Response, LlmError> {
    let resp = req.timeout(REQUEST_TIMEOUT).send().await.map_err(|e| {
        if e.is_builder() {
            LlmError::Invalid("invalid request".into())
        } else {
            LlmError::Transient(e.without_url().to_string())
        }
    })?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    Err(match status {
        StatusCode::TOO_MANY_REQUESTS => LlmError::RateLimited {
            retry_after: retry_after(&resp),
        },
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => LlmError::Unauthorized,
        s if s.is_server_error() => LlmError::Transient(format!("provider status {s}")),
        StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::UNPROCESSABLE_ENTITY => {
            // Body error bisa gagal dibaca/di-parse; itu tidak mengubah klasifikasi.
            let body = resp.json::<Value>().await.unwrap_or(Value::Null);
            if is_bad_api_key(&body) {
                return Err(LlmError::Unauthorized);
            }
            LlmError::Invalid(format!(
                "provider status {status}{}",
                provider_message(&body)
            ))
        }
        other => LlmError::Invalid(format!("unexpected provider status {other}")),
    })
}

/// `Retry-After` dalam detik. Bentuk HTTP-date diabaikan.
fn retry_after(resp: &Response) -> Option<Duration> {
    let secs = resp.headers().get("retry-after")?.to_str().ok()?;
    let secs = secs.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER))
}

/// Gemini menjawab key salah dengan HTTP 400, bukan 401: `error.details[].reason ==
/// "API_KEY_INVALID"`, atau `error.status == "INVALID_ARGUMENT"` dengan pesan "API key".
fn is_bad_api_key(body: &Value) -> bool {
    let error = &body["error"];
    let by_reason = error["details"]
        .as_array()
        .is_some_and(|d| d.iter().any(|x| x["reason"] == "API_KEY_INVALID"));
    let by_status = error["status"] == "INVALID_ARGUMENT"
        && error["message"]
            .as_str()
            .is_some_and(|m| m.contains("API key"));
    by_reason || by_status
}

/// `: <error.message>` dari body error provider (OpenAI, Anthropic, Gemini sama-sama
/// memakai `error.message`), dipotong 200 karakter. Kosong kalau tidak ada.
fn provider_message(body: &Value) -> String {
    match body["error"]["message"].as_str() {
        Some(m) if !m.is_empty() => {
            let short: String = m.chars().take(MAX_PROVIDER_MESSAGE).collect();
            format!(": {short}")
        }
        _ => String::new(),
    }
}

/// Header berisi API key, ditandai sensitif supaya tidak muncul di log/Debug.
/// Key dengan karakter yang tidak sah di header jelas bukan key valid -> `Unauthorized`.
pub(crate) fn key_header(api_key: &str) -> Result<HeaderValue, LlmError> {
    let mut value = HeaderValue::from_str(api_key).map_err(|_| LlmError::Unauthorized)?;
    value.set_sensitive(true);
    Ok(value)
}

/// Decode body sukses ke `T`; kegagalan jadi `Invalid` tanpa detail body.
pub(crate) async fn decode<T: serde::de::DeserializeOwned>(resp: Response) -> Result<T, LlmError> {
    // Baca byte dulu: kegagalan baca (koneksi putus, timeout) bukan salah respons -> Transient.
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| LlmError::Transient(e.without_url().to_string()))?;
    serde_json::from_slice(&bytes).map_err(|_| LlmError::Invalid("malformed response".into()))
}

/// Teks hasil harus tidak kosong.
pub(crate) fn non_empty(text: String) -> Result<String, LlmError> {
    if text.trim().is_empty() {
        Err(LlmError::Invalid("empty response".into()))
    } else {
        Ok(text)
    }
}
