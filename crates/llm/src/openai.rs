//! Client OpenAI dan semua server yang kompatibel (`/chat/completions`).

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use crate::types::{ChatRequest, ChatResponse, LlmClient, LlmError, decode, non_empty, send};

pub struct OpenAiClient {
    http: Client,
    base_url: String,
}

impl OpenAiClient {
    pub fn new(http: Client, base_url: &str) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
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
        let body = json!({
            "model": req.model,
            "messages": [
                {"role": "system", "content": req.system},
                {"role": "user", "content": req.user},
            ],
            "max_tokens": req.max_tokens,
        });
        let url = format!("{}/chat/completions", self.base_url);
        let resp = send(self.http.post(url).bearer_auth(api_key).json(&body)).await?;
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
