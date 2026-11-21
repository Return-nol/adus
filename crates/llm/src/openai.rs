//! Client OpenAI dan semua server yang kompatibel (`/chat/completions`).

use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::AUTHORIZATION;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::types::{
    AssistantPart, ChatRequest, ChatResponse, LlmClient, LlmError, Message, ReasoningEffort,
    StopReason, ToolCall, UserPart, decode, format_provider_message, is_reasoning_unsupported,
    key_header, non_empty, normalize_model_ids, request_timeout, send,
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
    error: Option<WireError>,
}

#[derive(Deserialize)]
struct WireError {
    message: Option<String>,
    code: Option<Value>,
}

#[derive(Deserialize)]
struct Choice {
    message: WireMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct WireMessage {
    content: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
    // Some compatible servers send `"tool_calls": null`, hence `Option`.
    tool_calls: Option<Vec<WireToolCall>>,
}

#[derive(Deserialize)]
struct WireToolCall {
    id: String,
    function: WireFunction,
}

#[derive(Deserialize)]
struct WireFunction {
    name: String,
    /// A JSON-encoded string per the OpenAI API.
    arguments: Option<Value>,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    cached_tokens: Option<u32>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ModelsPayload {
    Wrapped { data: Vec<ModelItem> },
    List(Vec<ModelItem>),
}

#[derive(Deserialize)]
struct ModelItem {
    id: String,
}

impl OpenAiClient {
    async fn chat_once(
        &self,
        api_key: &str,
        req: &ChatRequest,
        effort: Option<ReasoningEffort>,
    ) -> Result<ChatResponse, LlmError> {
        let mut body = json!({
            "model": req.model,
            "messages": wire_messages(req),
        });
        // Reasoning tokens count against the same cap: reserve room for them.
        let reserve = effort.map_or(0, ReasoningEffort::output_reserve);
        body[self.max_tokens_field] = req.max_tokens.saturating_add(reserve).into();
        if let Some(e) = effort {
            body["reasoning_effort"] = e.as_str().into();
        }
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({"type": "function", "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }})
                })
                .collect();
            body["tools"] = tools.into();
            body["tool_choice"] = "auto".into();
        }
        let url = format!("{}/chat/completions", self.base_url);
        let bearer = key_header(&format!("Bearer {api_key}"))?;
        let resp = send(
            self.http
                .post(url)
                .header(AUTHORIZATION, bearer)
                .json(&body),
            !req.tools.is_empty(),
            request_timeout(effort),
        )
        .await?;
        let completion: Completion = decode(resp).await?;
        if completion.choices.is_empty()
            && let Some(err) = completion.error
        {
            return Err(wire_error(err));
        }
        let (message, finish_reason) = completion
            .choices
            .into_iter()
            .next()
            .map_or((None, None), |c| (Some(c.message), c.finish_reason));
        let has_reasoning = message.as_ref().is_some_and(|m| {
            [m.reasoning.as_deref(), m.reasoning_content.as_deref()]
                .into_iter()
                .flatten()
                .any(|s| !s.trim().is_empty())
        });
        let (text, wire_calls) = message.map_or((None, None), |m| (m.content, m.tool_calls));
        let tool_calls: Vec<ToolCall> = wire_calls
            .unwrap_or_default()
            .into_iter()
            .map(|c| ToolCall {
                id: c.id,
                name: c.function.name,
                arguments: parse_arguments(c.function.arguments),
                provider_meta: None,
            })
            .collect();
        let text = text.unwrap_or_default();
        if text.trim().is_empty()
            && tool_calls.is_empty()
            && finish_reason.as_deref() == Some("length")
            && has_reasoning
        {
            return Err(LlmError::Invalid(
                "empty response: model exhausted max_tokens during reasoning".into(),
            ));
        }
        let usage = completion.usage;
        Ok(ChatResponse {
            text: non_empty(text, &tool_calls)?,
            tool_calls,
            stop_reason: stop_reason(finish_reason.as_deref()),
            input_tokens: usage.as_ref().and_then(|u| u.prompt_tokens).unwrap_or(0),
            output_tokens: usage
                .as_ref()
                .and_then(|u| u.completion_tokens)
                .unwrap_or(0),
            cache_read_tokens: usage
                .as_ref()
                .and_then(|u| u.prompt_tokens_details.as_ref())
                .and_then(|d| d.cached_tokens)
                .unwrap_or(0),
            cache_write_tokens: 0,
        })
    }
}

#[async_trait]
impl LlmClient for OpenAiClient {
    async fn chat(&self, api_key: &str, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        match self.chat_once(api_key, req, req.effort).await {
            Err(ref err) if req.effort.is_some() && is_reasoning_unsupported(err) => {
                self.chat_once(api_key, req, None).await
            }
            other => other,
        }
    }

    async fn list_models(&self, api_key: &str) -> Result<Vec<String>, LlmError> {
        let url = format!("{}/models", self.base_url);
        let bearer = key_header(&format!("Bearer {api_key}"))?;
        let resp = send(
            self.http.get(url).header(AUTHORIZATION, bearer),
            false,
            request_timeout(None),
        )
        .await?;
        let payload: ModelsPayload = decode(resp).await?;
        let items = match payload {
            ModelsPayload::Wrapped { data } | ModelsPayload::List(data) => data,
        };
        Ok(normalize_model_ids(items.into_iter().map(|m| m.id)))
    }
}

/// System prompt first (always sent, even when empty), then the conversation.
fn wire_messages(req: &ChatRequest) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": req.system})];
    for message in &req.messages {
        match message {
            Message::User { parts } => {
                let text: Vec<&str> = parts.iter().map(|UserPart::Text(t)| t.as_str()).collect();
                out.push(json!({"role": "user", "content": text.join("\n\n")}));
            }
            Message::Assistant { parts } => out.push(assistant_message(parts)),
            // One `tool` message per result.
            Message::Tool { results } => {
                out.extend(results.iter().map(
                    |r| json!({"role": "tool", "tool_call_id": r.call_id, "content": r.content}),
                ))
            }
        }
    }
    out
}

fn assistant_message(parts: &[AssistantPart]) -> Value {
    let mut text = String::new();
    let mut calls = Vec::new();
    for part in parts {
        match part {
            AssistantPart::Text(t) => text.push_str(t),
            AssistantPart::ToolCall(c) => calls.push(json!({
                "id": c.id,
                "type": "function",
                "function": {"name": c.name, "arguments": c.arguments.to_string()},
            })),
        }
    }
    let content = if text.is_empty() {
        Value::Null
    } else {
        text.into()
    };
    let mut message = json!({"role": "assistant", "content": content});
    if !calls.is_empty() {
        message["tool_calls"] = calls.into();
    }
    message
}

/// Arguments arrive as a JSON string. Malformed JSON does not fail the response: it is wrapped
/// so tool dispatch can report the error and the model can retry on its next turn.
fn parse_arguments(raw: Option<Value>) -> Value {
    match raw {
        Some(Value::String(s)) if s.trim().is_empty() => json!({}),
        Some(Value::String(s)) => serde_json::from_str(&s)
            .unwrap_or_else(|err| json!({"__raw_parse_error": err.to_string(), "raw": s})),
        // Some compatible servers already send an object.
        Some(Value::Null) | None => json!({}),
        Some(other) => other,
    }
}

fn stop_reason(finish_reason: Option<&str>) -> StopReason {
    match finish_reason {
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some("stop") => StopReason::EndTurn,
        _ => StopReason::Other,
    }
}

/// Maps an OpenRouter-style `{"error": ...}` payload sent inside an HTTP 200 completion body.
fn wire_error(err: WireError) -> LlmError {
    let code = err
        .code
        .as_ref()
        .and_then(Value::as_u64)
        .and_then(|n| u16::try_from(n).ok());
    let suffix = err
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map_or_else(String::new, format_provider_message);
    match code {
        Some(401 | 403) => LlmError::Unauthorized,
        Some(429) => LlmError::RateLimited { retry_after: None },
        Some(c) if (500..=599).contains(&c) => {
            LlmError::Transient(format!("provider status {c}{suffix}"))
        }
        Some(c) => LlmError::Invalid(format!("provider status {c}{suffix}")),
        None => LlmError::Invalid(format!("provider error{suffix}")),
    }
}
