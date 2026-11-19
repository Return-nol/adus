pub mod anthropic;
pub mod gemini;
pub mod openai;
pub mod types;

use async_trait::async_trait;

pub use adus_core::types::ProviderKind;
pub use anthropic::AnthropicClient;
pub use gemini::GeminiClient;
pub use openai::OpenAiClient;
pub use types::{ChatRequest, ChatResponse, LlmClient, LlmError};

/// Base URL bawaan tiap provider. `OpenaiCompatible` tidak punya (wajib diisi admin).
pub fn default_base_url(kind: ProviderKind) -> Option<&'static str> {
    match kind {
        ProviderKind::Openai => Some("https://api.openai.com/v1"),
        ProviderKind::Anthropic => Some("https://api.anthropic.com"),
        ProviderKind::Gemini => Some("https://generativelanguage.googleapis.com"),
        ProviderKind::OpenaiCompatible => None,
    }
}

/// `base_url` yang diisi menimpa default. Untuk `OpenaiCompatible` tanpa `base_url`, client
/// yang dikembalikan selalu gagal dengan `Invalid` (tidak panic).
pub fn client_for(
    kind: ProviderKind,
    base_url: Option<&str>,
    http: reqwest::Client,
) -> Box<dyn LlmClient> {
    let Some(base) = base_url.or(default_base_url(kind)) else {
        return Box::new(MissingBaseUrl);
    };
    match kind {
        ProviderKind::Openai | ProviderKind::OpenaiCompatible => {
            Box::new(OpenAiClient::new(http, base))
        }
        ProviderKind::Anthropic => Box::new(AnthropicClient::new(http, base)),
        ProviderKind::Gemini => Box::new(GeminiClient::new(http, base)),
    }
}

struct MissingBaseUrl;

#[async_trait]
impl LlmClient for MissingBaseUrl {
    async fn chat(&self, _api_key: &str, _req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        Err(LlmError::Invalid(
            "base_url required for openai_compatible".into(),
        ))
    }
}
