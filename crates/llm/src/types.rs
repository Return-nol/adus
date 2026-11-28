//! Tipe bersama semua client LLM, plus satu pintu kirim request (`send`).

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::HeaderValue;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Timeout satu request ke provider. Dipasang per request supaya `reqwest::Client` bisa dibagi.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Timeout for a request that asks for reasoning: thinking tokens add minutes of generation.
const REASONING_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// Batas panjang pesan error provider yang kita teruskan.
const MAX_PROVIDER_MESSAGE: usize = 200;
/// Batas atas `Retry-After`; mencegah overflow `Instant + Duration` di pemanggil.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

/// Effort level for reasoning/thinking models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Output tokens reserved for reasoning on top of the answer's `max_tokens` (also used as
    /// `thinkingBudget` on Gemini 2.5 / unversioned Gemini models and legacy Claude models).
    /// Providers count reasoning tokens against the same output cap, so without this a long think
    /// leaves no room for the answer.
    pub(crate) fn output_reserve(self) -> u32 {
        match self {
            Self::Low => 4_096,
            Self::Medium => 12_000,
            Self::High => 23_000,
        }
    }
}

/// Per-request timeout: longer when the request asks for reasoning.
pub(crate) fn request_timeout(effort: Option<ReasoningEffort>) -> Duration {
    match effort {
        Some(_) => REASONING_REQUEST_TIMEOUT,
        None => REQUEST_TIMEOUT,
    }
}

/// One LLM call: a system prompt, the conversation so far, and the tools the model may call.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    /// Empty: no tools are sent, and an error is never reported as `ToolsUnsupported`.
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    pub effort: Option<ReasoningEffort>,
}

impl ChatRequest {
    /// Single-turn request: one user message, no tools.
    pub fn text(
        model: impl Into<String>,
        system: impl Into<String>,
        user: impl Into<String>,
        max_tokens: u32,
    ) -> Self {
        Self {
            model: model.into(),
            system: system.into(),
            messages: vec![Message::User {
                parts: vec![UserPart::Text(user.into())],
            }],
            tools: Vec::new(),
            max_tokens,
            effort: None,
        }
    }

    pub fn with_effort(mut self, effort: ReasoningEffort) -> Self {
        self.effort = Some(effort);
        self
    }

    /// Number of tool calls the assistant already made in this conversation.
    pub(crate) fn prior_tool_calls(&self) -> usize {
        self.messages
            .iter()
            .map(|m| match m {
                Message::Assistant { parts } => parts
                    .iter()
                    .filter(|p| matches!(p, AssistantPart::ToolCall(_)))
                    .count(),
                Message::User { .. } | Message::Tool { .. } => 0,
            })
            .sum()
    }
}

/// One turn of the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        parts: Vec<UserPart>,
    },
    Assistant {
        parts: Vec<AssistantPart>,
    },
    /// Results of the tool calls from the previous assistant turn.
    Tool {
        results: Vec<ToolResult>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UserPart {
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AssistantPart {
    Text(String),
    ToolCall(ToolCall),
    /// Opaque per-turn provider metadata (JSON object), replayed verbatim by the client that produced it.
    ProviderMeta(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider call id; Gemini has none, so its client assigns `gc_{prior}_{idx}`.
    pub id: String,
    pub name: String,
    pub arguments: Value,
    /// Opaque provider metadata (Gemini `thoughtSignature`), sent back verbatim on the next turn.
    pub provider_meta: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub tool_name: String,
    pub content: String,
    pub is_error: bool,
}

/// A tool the model may call; `parameters` is a JSON Schema object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    /// Total input tokens, cached ones included.
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
    /// Opaque per-turn provider metadata captured from the response, if any.
    pub provider_meta: Option<String>,
}

impl ChatResponse {
    /// The model's visible reasoning for this turn, for debug logging: OpenAI-compatible
    /// `reasoning_content` / `reasoning` / `reasoning_details` (kept in `provider_meta`) and
    /// Anthropic thinking blocks (kept on the first tool call). `None` when the provider returned
    /// none (Gemini thoughts are not requested).
    pub fn reasoning(&self) -> Option<String> {
        let metas = self.provider_meta.iter().chain(
            self.tool_calls
                .iter()
                .filter_map(|c| c.provider_meta.as_ref()),
        );
        let mut out: Vec<String> = Vec::new();
        for meta in metas {
            if let Ok(value) = serde_json::from_str::<Value>(meta) {
                collect_reasoning(&value, &mut out);
            }
        }
        out.dedup();
        (!out.is_empty()).then(|| out.join("\n"))
    }
}

/// Reasoning strings in one provider-meta value (see [`ChatResponse::reasoning`]).
fn collect_reasoning(value: &Value, out: &mut Vec<String>) {
    let mut push = |s: Option<&str>| {
        if let Some(s) = s.map(str::trim).filter(|s| !s.is_empty()) {
            out.push(s.to_owned());
        }
    };
    match value {
        Value::Array(items) => {
            for item in items {
                // Anthropic `{"type":"thinking","thinking":..}`; OpenRouter `reasoning_details`
                // entries carry `text` or `summary`.
                push(item.get("thinking").and_then(Value::as_str));
                push(item.get("text").and_then(Value::as_str));
                push(item.get("summary").and_then(Value::as_str));
            }
        }
        Value::Object(map) => {
            // `reasoning` usually repeats `reasoning_content`; `dedup` drops the copy.
            push(map.get("reasoning_content").and_then(Value::as_str));
            push(map.get("reasoning").and_then(Value::as_str));
            if map.get("reasoning_content").is_none()
                && map.get("reasoning").is_none()
                && let Some(details) = map.get("reasoning_details")
            {
                collect_reasoning(details, out);
            }
        }
        _ => {}
    }
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
    /// The model or endpoint rejected the `tools` in the request.
    #[error("tools not supported: {0}")]
    ToolsUnsupported(String),
    /// The prompt does not fit the model's context window (or the request is too large).
    #[error("context window exceeded: {0}")]
    ContextOverflow(String),
}

impl LlmError {
    /// Canonical snake_case error kind stored in `llm_usage.error_kind`.
    pub fn error_kind(&self) -> &'static str {
        match self {
            Self::RateLimited { .. } => "rate_limited",
            Self::Unauthorized => "unauthorized",
            Self::Transient(_) => "transient",
            Self::Invalid(_) => "invalid",
            Self::ToolsUnsupported(_) => "tools_unsupported",
            Self::ContextOverflow(_) => "context_overflow",
        }
    }
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn chat(&self, api_key: &str, req: &ChatRequest) -> Result<ChatResponse, LlmError>;
    async fn list_models(&self, api_key: &str) -> Result<Vec<String>, LlmError>;
}

/// Substrings (lowercase) in a provider error that mean the model rejected reasoning/thinking params.
const REASONING_UNSUPPORTED_MARKERS: [&str; 6] = [
    "reasoning_effort",
    "reasoning effort",
    "thinking",
    "budget_tokens",
    "output_config",
    "does not support reasoning",
];

/// Substrings (lowercase) in a provider error naming a max-tokens parameter.
const MAX_TOKENS_PARAM_MARKERS: [&str; 3] =
    ["max_tokens", "max_completion_tokens", "maxoutputtokens"];
/// Substrings (lowercase) in a provider error indicating an exceeded or out-of-range limit.
const MAX_TOKENS_LIMIT_MARKERS: [&str; 7] = [
    "exceed",
    "maximum",
    "at most",
    "must be <=",
    "less than",
    "too large",
    " > ",
];

fn invalid_non_404_lower(err: &LlmError) -> Option<String> {
    let LlmError::Invalid(msg) = err else {
        return None;
    };
    let lower = msg.to_lowercase();
    (!lower.starts_with("provider status 404")).then_some(lower)
}

fn is_reasoning_unsupported(err: &LlmError) -> bool {
    invalid_non_404_lower(err).is_some_and(|lower| {
        REASONING_UNSUPPORTED_MARKERS
            .iter()
            .any(|m| lower.contains(m))
    })
}

fn is_max_tokens_exceeded(err: &LlmError) -> bool {
    invalid_non_404_lower(err).is_some_and(|lower| {
        MAX_TOKENS_PARAM_MARKERS.iter().any(|m| lower.contains(m))
            && MAX_TOKENS_LIMIT_MARKERS.iter().any(|m| lower.contains(m))
    })
}

/// Retries `call` once with `effort = None` when the provider rejects the reasoning parameter
/// or rejects the reasoning-inflated output token cap.
pub(crate) async fn chat_with_effort_fallback<F, Fut>(
    effort: Option<ReasoningEffort>,
    call: F,
) -> Result<ChatResponse, LlmError>
where
    F: Fn(Option<ReasoningEffort>) -> Fut,
    Fut: std::future::Future<Output = Result<ChatResponse, LlmError>>,
{
    match call(effort).await {
        Err(ref err)
            if effort.is_some()
                && (is_reasoning_unsupported(err) || is_max_tokens_exceeded(err)) =>
        {
            call(None).await
        }
        other => other,
    }
}

pub(crate) fn normalize_model_ids(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = ids
        .into_iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Substrings (lowercase) in a provider error that mean the model or endpoint cannot use tools.
const TOOLS_UNSUPPORTED_MARKERS: [&str; 5] = [
    "does not support tools",
    "tool_use is not supported",
    "tools is not supported",
    "function calling is not supported",
    "tool calling is not supported",
];
/// Substrings (lowercase) in a provider error that mean the prompt is too long for the model.
const CONTEXT_OVERFLOW_MARKERS: [&str; 6] = [
    "context_length_exceeded",
    "maximum context length",
    "prompt is too long",
    "exceeds the maximum number of tokens",
    "token count exceeds",
    "input is too long",
];

/// Kirim request lalu petakan status gagal ke `LlmError`.
/// API key, URL, dan body request tidak pernah masuk pesan error.
/// `has_tools`: the request carries tools, so a tools rejection becomes `ToolsUnsupported`.
/// `timeout`: from [`request_timeout`] for chat calls, `REQUEST_TIMEOUT` for the rest.
pub(crate) async fn send(
    req: RequestBuilder,
    has_tools: bool,
    timeout: Duration,
) -> Result<Response, LlmError> {
    let resp = req.timeout(timeout).send().await.map_err(|e| {
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
        StatusCode::BAD_REQUEST
        | StatusCode::NOT_FOUND
        | StatusCode::UNPROCESSABLE_ENTITY
        | StatusCode::PAYLOAD_TOO_LARGE => {
            // Body error bisa gagal dibaca/di-parse; itu tidak mengubah klasifikasi.
            let raw = resp.bytes().await.unwrap_or_default();
            let body = serde_json::from_slice::<Value>(&raw).unwrap_or(Value::Null);
            if is_bad_api_key(&body) {
                return Err(LlmError::Unauthorized);
            }
            let message = format!("provider status {status}{}", provider_message(&body));
            let lower = error_text(&body);
            let too_large = status == StatusCode::PAYLOAD_TOO_LARGE;
            if has_tools && !too_large && tools_unsupported(&lower) {
                LlmError::ToolsUnsupported(message)
            } else if too_large || (status != StatusCode::NOT_FOUND && context_overflow(&lower)) {
                LlmError::ContextOverflow(message)
            } else {
                LlmError::Invalid(message)
            }
        }
        other => LlmError::Invalid(format!("unexpected provider status {other}")),
    })
}

/// Lowercased `error.message`, `error.code`, `error.type`, or a plain-string `error`, one per
/// line. Only these fields are scanned for markers: some servers echo the request input
/// elsewhere in the body, and that input (PR code) may contain the marker phrases.
fn error_text(body: &Value) -> String {
    let error = &body["error"];
    [&error["message"], &error["code"], &error["type"], error]
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase()
}

fn tools_unsupported(lower_error: &str) -> bool {
    TOOLS_UNSUPPORTED_MARKERS
        .iter()
        .any(|m| lower_error.contains(m))
        || (lower_error.contains("unknown field") && lower_error.contains("tools"))
}

fn context_overflow(lower_error: &str) -> bool {
    CONTEXT_OVERFLOW_MARKERS
        .iter()
        .any(|m| lower_error.contains(m))
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
/// Some OpenAI-compatible servers (e.g. Ollama) send `error` as a plain string instead.
fn provider_message(body: &Value) -> String {
    let error = &body["error"];
    match error["message"].as_str().or_else(|| error.as_str()) {
        Some(m) if !m.is_empty() => format_provider_message(m),
        _ => String::new(),
    }
}

pub(crate) fn format_provider_message(msg: &str) -> String {
    let short: String = msg.chars().take(MAX_PROVIDER_MESSAGE).collect();
    format!(": {short}")
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

/// A response must carry text or at least one tool call.
pub(crate) fn non_empty(text: String, tool_calls: &[ToolCall]) -> Result<String, LlmError> {
    if text.trim().is_empty() && tool_calls.is_empty() {
        Err(LlmError::Invalid("empty response".into()))
    } else {
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn response(meta: Option<Value>, call_meta: Option<Value>) -> ChatResponse {
        ChatResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: json!({}),
                provider_meta: call_meta.map(|v| v.to_string()),
            }],
            stop_reason: StopReason::ToolUse,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            provider_meta: meta.map(|v| v.to_string()),
        }
    }

    #[test]
    fn reasoning_reads_each_provider_format() {
        let cases = [
            // OpenAI-compatible: `reasoning` repeating `reasoning_content` is shown once.
            (
                response(
                    Some(json!({"reasoning_content": "a", "reasoning": "a"})),
                    None,
                ),
                Some("a"),
            ),
            // OpenRouter with only `reasoning_details`.
            (
                response(
                    Some(json!({"reasoning_details": [{"type": "reasoning.text", "text": "b"}]})),
                    None,
                ),
                Some("b"),
            ),
            // Anthropic thinking blocks on the first tool call; redacted blocks have no text.
            (
                response(
                    None,
                    Some(json!([
                        {"type": "thinking", "thinking": "c", "signature": "s"},
                        {"type": "redacted_thinking", "data": "x"}
                    ])),
                ),
                Some("c"),
            ),
            // Gemini `thoughtSignature` is an opaque string, not reasoning.
            (response(None, None), None),
        ];
        for (resp, expected) in cases {
            assert_eq!(resp.reasoning().as_deref(), expected);
        }
    }
}
