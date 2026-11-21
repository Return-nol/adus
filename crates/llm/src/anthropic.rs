//! Client Anthropic Messages API (`/v1/messages`).

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::types::{
    AssistantPart, ChatRequest, ChatResponse, LlmClient, LlmError, Message, ReasoningEffort,
    StopReason, ToolCall, UserPart, decode, is_reasoning_unsupported, key_header, non_empty,
    normalize_model_ids, send,
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
    stop_reason: Option<String>,
    usage: Option<Usage>,
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

fn thinking_budget(effort: ReasoningEffort) -> u32 {
    match effort {
        ReasoningEffort::Low => 2_048,
        ReasoningEffort::Medium => 4_096,
        ReasoningEffort::High => 8_192,
    }
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
    cache_creation_input_tokens: Option<u32>,
}

impl AnthropicClient {
    async fn chat_once(
        &self,
        api_key: &str,
        req: &ChatRequest,
        effort: Option<ReasoningEffort>,
    ) -> Result<ChatResponse, LlmError> {
        let mut max_tokens = req.max_tokens;
        let thinking = effort.map(|e| {
            let budget = thinking_budget(e);
            max_tokens = req.max_tokens.saturating_add(budget);
            json!({"type": "enabled", "budget_tokens": budget})
        });
        let mut body = json!({
            "model": req.model,
            "max_tokens": max_tokens,
            "messages": wire_messages(&req.messages),
        });
        if let Some(thinking) = thinking {
            body["thinking"] = thinking;
        }
        // Up to three cache breakpoints (system, last tool, last user turn); the API allows four.
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
        let request = self
            .http
            .post(url)
            .header("x-api-key", key_header(api_key)?)
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        let messages: Messages = decode(send(request, !req.tools.is_empty()).await?).await?;
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
                Block::Other => {}
            }
        }
        if !thinking_blocks.is_empty()
            && let Some(first) = tool_calls.first_mut()
        {
            first.provider_meta = serde_json::to_string(&thinking_blocks).ok();
        }
        let usage = messages.usage;
        let field = |f: fn(&Usage) -> Option<u32>| usage.as_ref().and_then(f).unwrap_or(0);
        let cache_read = field(|u| u.cache_read_input_tokens);
        let cache_write = field(|u| u.cache_creation_input_tokens);
        Ok(ChatResponse {
            text: non_empty(text, &tool_calls)?,
            tool_calls,
            stop_reason: stop_reason(messages.stop_reason.as_deref()),
            // Anthropic reports cached input separately; the total keeps usage comparable.
            input_tokens: field(|u| u.input_tokens)
                .saturating_add(cache_read)
                .saturating_add(cache_write),
            output_tokens: field(|u| u.output_tokens),
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
        })
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn chat(&self, api_key: &str, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        match self.chat_once(api_key, req, req.effort).await {
            Err(ref err) if req.effort.is_some() && is_reasoning_unsupported(err) => {
                self.chat_once(api_key, req, None).await
            }
            other => other,
        }
    }

    async fn list_models(&self, api_key: &str) -> Result<Vec<String>, LlmError> {
        let url = format!("{}/v1/models", self.base_url);
        let request = self
            .http
            .get(url)
            .header("x-api-key", key_header(api_key)?)
            .header("anthropic-version", "2023-06-01");
        let payload: ModelsPayload = decode(send(request, false).await?).await?;
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
