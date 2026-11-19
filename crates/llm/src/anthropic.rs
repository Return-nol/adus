//! Client Anthropic Messages API (`/v1/messages`).

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use crate::types::{
    ChatRequest, ChatResponse, LlmClient, LlmError, decode, key_header, non_empty, send,
};

pub struct AnthropicClient {
    http: Client,
    base_url: String,
}

impl AnthropicClient {
    pub fn new(http: Client, base_url: &str) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

#[derive(Deserialize)]
struct Messages {
    #[serde(default)]
    content: Vec<Block>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn chat(&self, api_key: &str, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let body = json!({
            "model": req.model,
            "system": req.system,
            "messages": [{"role": "user", "content": req.user}],
            "max_tokens": req.max_tokens,
        });
        let url = format!("{}/v1/messages", self.base_url);
        let request = self
            .http
            .post(url)
            .header("x-api-key", key_header(api_key)?)
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        let messages: Messages = decode(send(request).await?).await?;
        // Balasan bisa berisi beberapa blok; hanya blok "text" yang dipakai.
        let text: String = messages
            .content
            .into_iter()
            .filter(|b| b.kind == "text")
            .filter_map(|b| b.text)
            .collect();
        let usage = messages.usage;
        Ok(ChatResponse {
            text: non_empty(text)?,
            input_tokens: usage.as_ref().and_then(|u| u.input_tokens).unwrap_or(0),
            output_tokens: usage.as_ref().and_then(|u| u.output_tokens).unwrap_or(0),
        })
    }
}
