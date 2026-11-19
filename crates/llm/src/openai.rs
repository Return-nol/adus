//! Client OpenAI dan semua server yang kompatibel (`/chat/completions`).

use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::AUTHORIZATION;
use serde::Deserialize;
use serde_json::json;

use crate::types::{
    ChatRequest, ChatResponse, LlmClient, LlmError, decode, key_header, non_empty, send,
};

pub struct OpenAiClient {
    http: Client,
    base_url: String,
    /// Nama field batas token: OpenAI asli `max_completion_tokens`, server kompatibel `max_tokens`.
    max_tokens_field: &'static str,
}

impl OpenAiClient {
    pub fn new(http: Client, base_url: &str, max_tokens_field: &'static str) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            max_tokens_field,
        }
    }
}

#[derive(Deserialize)]
struct Completion {
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    content: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
}

#[async_trait]
impl LlmClient for OpenAiClient {
    async fn chat(&self, api_key: &str, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let mut body = json!({
            "model": req.model,
            "messages": [
                {"role": "system", "content": req.system},
                {"role": "user", "content": req.user},
            ],
        });
        body[self.max_tokens_field] = req.max_tokens.into();
        let url = format!("{}/chat/completions", self.base_url);
        let bearer = key_header(&format!("Bearer {api_key}"))?;
        let resp = send(
            self.http
                .post(url)
                .header(AUTHORIZATION, bearer)
                .json(&body),
        )
        .await?;
        let completion: Completion = decode(resp).await?;
        let text = completion
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();
        let usage = completion.usage;
        Ok(ChatResponse {
            text: non_empty(text)?,
            input_tokens: usage.as_ref().and_then(|u| u.prompt_tokens).unwrap_or(0),
            output_tokens: usage
                .as_ref()
                .and_then(|u| u.completion_tokens)
                .unwrap_or(0),
        })
    }
}
