//! Client Google Gemini (`generateContent`).

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use crate::types::{
    ChatRequest, ChatResponse, LlmClient, LlmError, decode, key_header, non_empty, send,
};

pub struct GeminiClient {
    http: Client,
    base_url: String,
}

impl GeminiClient {
    pub fn new(http: Client, base_url: &str) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Generated {
    #[serde(default)]
    candidates: Vec<Candidate>,
    prompt_feedback: Option<PromptFeedback>,
    usage_metadata: Option<Usage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptFeedback {
    block_reason: Option<String>,
}

#[derive(Deserialize)]
struct Candidate {
    content: Option<Content>,
}

#[derive(Deserialize)]
struct Content {
    #[serde(default)]
    parts: Vec<Part>,
}

#[derive(Deserialize)]
struct Part {
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Usage {
    prompt_token_count: Option<u32>,
    candidates_token_count: Option<u32>,
}

#[async_trait]
impl LlmClient for GeminiClient {
    async fn chat(&self, api_key: &str, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let body = json!({
            "systemInstruction": {"parts": [{"text": req.system}]},
            "contents": [{"role": "user", "parts": [{"text": req.user}]}],
            "generationConfig": {"maxOutputTokens": req.max_tokens},
        });
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url, req.model
        );
        let request = self
            .http
            .post(url)
            .header("x-goog-api-key", key_header(api_key)?)
            .json(&body);
        let generated: Generated = decode(send(request).await?).await?;
        if let Some(reason) = generated.prompt_feedback.and_then(|f| f.block_reason) {
            return Err(LlmError::Invalid(format!("blocked: {reason}")));
        }
        let text: String = generated
            .candidates
            .into_iter()
            .next()
            .and_then(|c| c.content)
            .map(|c| c.parts.into_iter().filter_map(|p| p.text).collect())
            .unwrap_or_default();
        let usage = generated.usage_metadata;
        Ok(ChatResponse {
            text: non_empty(text)?,
            input_tokens: usage
                .as_ref()
                .and_then(|u| u.prompt_token_count)
                .unwrap_or(0),
            output_tokens: usage
                .as_ref()
                .and_then(|u| u.candidates_token_count)
                .unwrap_or(0),
        })
    }
}
