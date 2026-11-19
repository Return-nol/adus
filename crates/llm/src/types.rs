//! Tipe bersama semua client LLM, plus satu pintu kirim request (`send`).

use std::time::Duration;

use async_trait::async_trait;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde_json::Value;

/// Timeout satu request ke provider. Dipasang per request supaya `reqwest::Client` bisa dibagi.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Batas panjang pesan error provider yang kita teruskan.
const MAX_PROVIDER_MESSAGE: usize = 200;

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
    let resp = req
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| LlmError::Transient(e.without_url().to_string()))?;
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
            let detail = provider_message(resp).await;
            LlmError::Invalid(format!("provider status {status}{detail}"))
        }
        other => LlmError::Invalid(format!("unexpected provider status {other}")),
    })
}

/// `Retry-After` dalam detik. Bentuk HTTP-date diabaikan.
fn retry_after(resp: &Response) -> Option<Duration> {
    let secs = resp.headers().get("retry-after")?.to_str().ok()?;
    secs.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// `: <error.message>` dari body error provider (OpenAI, Anthropic, Gemini sama-sama
/// memakai `error.message`), dipotong 200 karakter. Kosong kalau tidak ada.
async fn provider_message(resp: Response) -> String {
    let Ok(body) = resp.json::<Value>().await else {
        return String::new();
    };
    match body["error"]["message"].as_str() {
        Some(m) if !m.is_empty() => {
            let short: String = m.chars().take(MAX_PROVIDER_MESSAGE).collect();
            format!(": {short}")
        }
        _ => String::new(),
    }
}

/// Decode body sukses ke `T`; kegagalan jadi `Invalid` tanpa detail body.
pub(crate) async fn decode<T: serde::de::DeserializeOwned>(resp: Response) -> Result<T, LlmError> {
    resp.json::<T>()
        .await
        .map_err(|_| LlmError::Invalid("malformed response".into()))
}

/// Teks hasil harus tidak kosong.
pub(crate) fn non_empty(text: String) -> Result<String, LlmError> {
    if text.trim().is_empty() {
        Err(LlmError::Invalid("empty response".into()))
    } else {
        Ok(text)
    }
}
