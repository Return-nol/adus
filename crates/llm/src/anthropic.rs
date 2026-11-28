//! Client Anthropic Messages API (`/v1/messages`).

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::types::{
    AssistantPart, ChatRequest, ChatResponse, LlmClient, LlmError, Message, ReasoningEffort,
    StopReason, ToolCall, UserPart, chat_with_effort_fallback, decode, key_header,
    normalize_model_ids, request_timeout, send,
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
    /// The model that produced the reply: a fallback model when a refusal fell back.
    model: Option<String>,
    stop_reason: Option<String>,
    stop_details: Option<StopDetails>,
    usage: Option<Usage>,
}

/// Set on a `refusal`: the policy category that declined the request (may be absent).
#[derive(Deserialize)]
struct StopDetails {
    category: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text {
        text: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: String,
    },
    RedactedThinking {
        #[serde(default)]
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// A server-side fallback switch point: what comes before it is the declined attempt.
    Fallback {},
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ModelsPayload {
    #[serde(default)]
    data: Vec<ModelItem>,
}

#[derive(Deserialize)]
struct ModelItem {
    id: String,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
    cache_creation_input_tokens: Option<u32>,
}

/// The family (`opus`, `sonnet`, `haiku`, `fable`, ...) and `(major, minor)` version of a Claude
/// model id; `None` for an id without `claude-`, a `None` version when the id names none. A
/// minor part longer than two digits is a date suffix, not a minor version.
fn claude_model(model: &str) -> Option<(&str, Option<(u32, u32)>)> {
    let (_, rest) = model.split_once("claude-")?;
    let mut parts = rest.split(['-', '.']);
    let family = parts.next().unwrap_or_default();
    let version = parts
        .next()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u32>().ok())
        .map(|major| {
            let minor = parts
                .next()
                .filter(|s| (1..=2).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            (major, minor)
        });
    Some((family, version))
}

/// Claude 4.6+ and non-(opus|sonnet|haiku) Claude models use adaptive thinking and
/// `output_config.effort`; older Claude models and non-Claude proxies use `budget_tokens`.
fn adaptive_thinking(model: &str) -> bool {
    if model.contains("claude-3") {
        return false;
    }
    match claude_model(model) {
        None => false,
        Some(("opus" | "sonnet" | "haiku", Some(version))) => version > (4, 5),
        Some(_) => true,
    }
}

/// Models whose safety classifiers can decline a request and that accept the server-side
/// `fallbacks: "default"` opt-in: Claude Fable, Claude Opus 5+ and Claude Sonnet 5.5+. Claude
/// Haiku has no server-side fallback.
fn server_fallback(model: &str) -> bool {
    match claude_model(model) {
        Some(("fable", _)) => true,
        Some(("opus", Some(version))) => version >= (5, 0),
        Some(("sonnet", Some(version))) => version >= (5, 5),
        _ => false,
    }
}

/// Beta header of the server-side refusal fallback (`fallbacks: "default"`).
const SERVER_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

impl AnthropicClient {
    async fn chat_once(
        &self,
        api_key: &str,
        req: &ChatRequest,
        effort: Option<ReasoningEffort>,
    ) -> Result<ChatResponse, LlmError> {
        let max_tokens = match effort {
            Some(e) => req.max_tokens.saturating_add(e.output_reserve()),
            None => req.max_tokens,
        };
        let mut body = json!({
            "model": req.model,
            "max_tokens": max_tokens,
            "messages": wire_messages(&req.messages),
        });
        if let Some(e) = effort {
            if adaptive_thinking(&req.model) {
                // "summarized": Claude 4.7+ / 5.x return empty thinking blocks by default; this changes only what the response shows, not cost or behaviour, and lets the trace show reasoning.
                body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
                body["output_config"] = json!({"effort": e.as_str()});
            } else {
                body["thinking"] = json!({"type": "enabled", "budget_tokens": e.output_reserve()});
            }
        }
        // Up to three cache breakpoints (system, last tool, last user turn); the API allows four.
        let fallback = server_fallback(&req.model);
        if fallback {
            // A request the safety classifiers decline is re-run on Anthropic's recommended
            // fallback model (routed by refusal category) instead of coming back as a refusal.
            body["fallbacks"] = json!("default");
        }
        if !req.system.is_empty() {
            body["system"] = json!([{
                "type": "text",
                "text": req.system,
                "cache_control": ephemeral(),
            }]);
        }
        if !req.tools.is_empty() {
            let mut tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect();
            if let Some(last) = tools.last_mut() {
                last["cache_control"] = ephemeral();
            }
            body["tools"] = tools.into();
        }
        let url = format!("{}/v1/messages", self.base_url);
        let mut request = self
            .http
            .post(url)
            .header("x-api-key", key_header(api_key)?)
            .header("anthropic-version", "2023-06-01");
        if fallback {
            request = request.header("anthropic-beta", SERVER_FALLBACK_BETA);
        }
        let request = request.json(&body);
        let messages: Messages =
            decode(send(request, !req.tools.is_empty(), request_timeout(effort)).await?).await?;
        let mut text = String::new();
        let mut thinking_blocks = Vec::new();
        let mut tool_calls = Vec::new();
        for block in messages.content {
            match block {
                Block::Text { text: t } => text.push_str(&t),
                Block::Thinking {
                    thinking,
                    signature,
                } => thinking_blocks.push(json!({
                    "type": "thinking",
                    "thinking": thinking,
                    "signature": signature,
                })),
                Block::RedactedThinking { data } => thinking_blocks.push(json!({
                    "type": "redacted_thinking",
                    "data": data,
                })),
                Block::ToolUse { id, name, input } => tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: input,
                    provider_meta: None,
                }),
                Block::Fallback {} => {
                    // Everything before a switch point is the declined attempt's partial output:
                    // never run or echo it.
                    text.clear();
                    thinking_blocks.clear();
                    tool_calls.clear();
                }
                Block::Other => {}
            }
        }
        if messages.stop_reason.as_deref() == Some("refusal") {
            // Content of a refusal is empty or partial; it is never a reply.
            let category = messages
                .stop_details
                .and_then(|d| d.category)
                .unwrap_or_else(|| "none".into());
            return Err(LlmError::Invalid(format!(
                "request declined (stop_reason refusal, category {category})"
            )));
        }
        if let Some(served) = messages.model.as_deref().filter(|m| *m != req.model) {
            tracing::info!(requested = %req.model, served, "a fallback model served a declined request");
        }
        if !thinking_blocks.is_empty()
            && let Some(first) = tool_calls.first_mut()
        {
            first.provider_meta = serde_json::to_string(&thinking_blocks).ok();
        }
        if text.trim().is_empty() && tool_calls.is_empty() {
            // The stop reason tells a reply cut short (`max_tokens`) apart from an empty turn.
            let reason = messages.stop_reason.as_deref().unwrap_or("none");
            return Err(LlmError::Invalid(format!(
                "empty response (stop_reason {reason})"
            )));
        }
        let usage = messages.usage;
        let field = |f: fn(&Usage) -> Option<u32>| usage.as_ref().and_then(f).unwrap_or(0);
        let cache_read = field(|u| u.cache_read_input_tokens);
        let cache_write = field(|u| u.cache_creation_input_tokens);
        Ok(ChatResponse {
            text,
            tool_calls,
            stop_reason: stop_reason(messages.stop_reason.as_deref()),
            // Anthropic reports cached input separately; the total keeps usage comparable.
            input_tokens: field(|u| u.input_tokens)
                .saturating_add(cache_read)
                .saturating_add(cache_write),
            output_tokens: field(|u| u.output_tokens),
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            provider_meta: None,
        })
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn chat(&self, api_key: &str, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        chat_with_effort_fallback(req.effort, |effort| self.chat_once(api_key, req, effort)).await
    }

    async fn list_models(&self, api_key: &str) -> Result<Vec<String>, LlmError> {
        let url = format!("{}/v1/models", self.base_url);
        let request = self
            .http
            .get(url)
            .header("x-api-key", key_header(api_key)?)
            .header("anthropic-version", "2023-06-01");
        let payload: ModelsPayload =
            decode(send(request, false, request_timeout(None)).await?).await?;
        Ok(normalize_model_ids(payload.data.into_iter().map(|m| m.id)))
    }
}

fn ephemeral() -> Value {
    json!({"type": "ephemeral"})
}

/// Conversation as Anthropic messages. Tool results travel in a `user` message, and adjacent
/// messages with the same role are merged, because the API rejects two `user` turns in a row.
/// In a merged turn the `tool_result` blocks come first (the API requires it), then the text.
/// With two or more messages, the last block of the last `user` turn gets a cache breakpoint.
fn wire_messages(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<(&str, Vec<Value>)> = Vec::new();
    for message in messages {
        let (role, blocks) = match message {
            Message::User { parts } => (
                "user",
                parts
                    .iter()
                    .map(|UserPart::Text(t)| json!({"type": "text", "text": t}))
                    .collect(),
            ),
            Message::Assistant { parts } => ("assistant", assistant_blocks(parts)),
            Message::Tool { results } => (
                "user",
                results
                    .iter()
                    .map(|r| {
                        json!({
                            "type": "tool_result",
                            "tool_use_id": r.call_id,
                            "content": r.content,
                            "is_error": r.is_error,
                        })
                    })
                    .collect(),
            ),
        };
        match out.last_mut() {
            Some((last_role, last_blocks)) if *last_role == role => {
                last_blocks.extend(blocks);
                // Stable: keeps the order within the tool results and within the text.
                last_blocks.sort_by_key(|b| b["type"] != "tool_result");
            }
            _ => out.push((role, blocks)),
        }
    }
    if messages.len() >= 2
        && let Some(block) = out
            .iter_mut()
            .rev()
            .find(|(role, _)| *role == "user")
            .and_then(|(_, blocks)| blocks.last_mut())
    {
        block["cache_control"] = ephemeral();
    }
    out.into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect()
}

fn assistant_blocks(parts: &[AssistantPart]) -> Vec<Value> {
    let mut out = Vec::new();
    // Thinking blocks must precede any tool_use blocks when replaying an extended-thinking turn.
    for part in parts {
        if let AssistantPart::ToolCall(call) = part
            && let Some(meta) = &call.provider_meta
            && let Ok(blocks) = serde_json::from_str::<Vec<Value>>(meta)
        {
            out.extend(blocks);
            break;
        }
    }
    for part in parts {
        match part {
            // The API rejects empty text blocks.
            AssistantPart::Text(t) if t.is_empty() => {}
            AssistantPart::Text(t) => out.push(json!({"type": "text", "text": t})),
            AssistantPart::ToolCall(c) => out.push(json!({
                "type": "tool_use",
                "id": c.id,
                "name": c.name,
                "input": c.arguments,
            })),
            AssistantPart::ProviderMeta(_) => {}
        }
    }
    out
}

fn stop_reason(stop_reason: Option<&str>) -> StopReason {
    match stop_reason {
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("end_turn" | "stop_sequence") => StopReason::EndTurn,
        _ => StopReason::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::{adaptive_thinking, server_fallback};

    #[test]
    fn thinking_mode_and_server_fallback_follow_model_generation() {
        for model in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-sonnet-5.5",
            "claude-haiku-5-5",
            "claude-fable-5-2",
            "claude-opus-4-6",
            "claude-opus-4.6",
            "claude-sonnet-4-6",
            "claude-opus-4-8",
        ] {
            assert!(adaptive_thinking(model), "{model} should use adaptive");
        }

        for model in [
            "claude-haiku-4-5",
            "claude-sonnet-4-5",
            "claude-sonnet-4.5",
            "claude-opus-4-5-20251101",
            "claude-opus-4-1",
            "claude-sonnet-4-20250514",
            "claude-3-7-sonnet-latest",
            "kimi-k2",
            "m-1",
        ] {
            assert!(!adaptive_thinking(model), "{model} should use budget");
        }

        for model in [
            "claude-sonnet-5-5",
            "claude-sonnet-5.5",
            "claude-opus-5",
            "claude-opus-5-5",
            "claude-fable-5-2",
        ] {
            assert!(server_fallback(model), "{model} should opt into fallbacks");
        }
        for model in [
            "claude-sonnet-5",
            "claude-sonnet-4-6",
            "claude-haiku-5-5",
            "claude-opus-4-8",
            "claude-opus-4-5-20251101",
            "claude-3-7-sonnet-latest",
            "kimi-k2",
        ] {
            assert!(
                !server_fallback(model),
                "{model} should not opt into fallbacks"
            );
        }
    }
}
