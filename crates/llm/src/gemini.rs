//! Client Google Gemini (`generateContent`).

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::types::{
    AssistantPart, ChatRequest, ChatResponse, LlmClient, LlmError, Message, ReasoningEffort,
    StopReason, ToolCall, UserPart, chat_with_effort_fallback, decode, key_header, non_empty,
    normalize_model_ids, request_timeout, send,
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
#[serde(rename_all = "camelCase")]
struct Candidate {
    content: Option<Content>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Content {
    #[serde(default)]
    parts: Vec<Part>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Part {
    text: Option<String>,
    #[serde(default)]
    thought: bool,
    function_call: Option<FunctionCall>,
    thought_signature: Option<String>,
}

#[derive(Deserialize)]
struct FunctionCall {
    name: String,
    args: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Usage {
    prompt_token_count: Option<u32>,
    candidates_token_count: Option<u32>,
    cached_content_token_count: Option<u32>,
}

#[derive(Deserialize)]
struct GeminiModels {
    #[serde(default)]
    models: Vec<GeminiModel>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiModel {
    name: String,
    supported_generation_methods: Option<Vec<String>>,
}

/// Gemini 3+ models take `thinkingLevel` rather than `thinkingBudget`.
fn thinking_level(model: &str) -> bool {
    let Some((_, rest)) = model.split_once("gemini-") else {
        return false;
    };
    let token = rest.split(['.', '-']).next().unwrap_or("");
    !token.is_empty()
        && token.bytes().all(|b| b.is_ascii_digit())
        && token.parse::<u32>().is_ok_and(|n| n >= 3)
}

fn thinking_config(model: &str, effort: ReasoningEffort) -> Value {
    if thinking_level(model) {
        let level = match effort {
            ReasoningEffort::Low => "LOW",
            ReasoningEffort::Medium if model.contains("gemini-3-pro-preview") => "HIGH",
            ReasoningEffort::Medium => "MEDIUM",
            ReasoningEffort::High => "HIGH",
        };
        json!({"thinkingLevel": level})
    } else {
        json!({"thinkingBudget": effort.output_reserve()})
    }
}

impl GeminiClient {
    async fn chat_once(
        &self,
        api_key: &str,
        req: &ChatRequest,
        effort: Option<ReasoningEffort>,
    ) -> Result<ChatResponse, LlmError> {
        // Thinking tokens count against maxOutputTokens: add the reserve on top of the answer.
        let reserve = effort.map(ReasoningEffort::output_reserve);
        let mut generation_config =
            json!({"maxOutputTokens": req.max_tokens.saturating_add(reserve.unwrap_or(0))});
        if let Some(e) = effort {
            generation_config["thinkingConfig"] = thinking_config(req.model.as_str(), e);
        }
        let mut body = json!({
            "systemInstruction": {"parts": [{"text": req.system}]},
            "contents": wire_contents(&req.messages),
            "generationConfig": generation_config,
        });
        if !req.tools.is_empty() {
            let declarations: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    })
                })
                .collect();
            body["tools"] = json!([{"functionDeclarations": declarations}]);
            body["toolConfig"] = json!({"functionCallingConfig": {"mode": "AUTO"}});
        }
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url, req.model
        );
        let request = self
            .http
            .post(url)
            .header("x-goog-api-key", key_header(api_key)?)
            .json(&body);
        let generated: Generated =
            decode(send(request, !req.tools.is_empty(), request_timeout(effort)).await?).await?;
        if let Some(reason) = generated.prompt_feedback.and_then(|f| f.block_reason) {
            return Err(LlmError::Invalid(format!("blocked: {reason}")));
        }
        let (parts, finish_reason) =
            generated
                .candidates
                .into_iter()
                .next()
                .map_or((Vec::new(), None), |c| {
                    (
                        c.content.map(|c| c.parts).unwrap_or_default(),
                        c.finish_reason,
                    )
                });
        // Gemini has no call ids; `prior` keeps the synthetic ones unique across the conversation.
        let prior = req.prior_tool_calls();
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for part in parts {
            if let Some(t) = part.text
                && !part.thought
            {
                text.push_str(&t);
            }
            if let Some(call) = part.function_call {
                tool_calls.push(ToolCall {
                    id: format!("gc_{prior}_{}", tool_calls.len()),
                    name: call.name,
                    arguments: call.args.unwrap_or_else(|| json!({})),
                    provider_meta: part.thought_signature,
                });
            }
        }
        let stop_reason = if tool_calls.is_empty() {
            stop_reason(finish_reason.as_deref())
        } else {
            StopReason::ToolUse
        };
        let usage = generated.usage_metadata;
        let field = |f: fn(&Usage) -> Option<u32>| usage.as_ref().and_then(f).unwrap_or(0);
        Ok(ChatResponse {
            text: non_empty(text, &tool_calls)?,
            tool_calls,
            stop_reason,
            input_tokens: field(|u| u.prompt_token_count),
            output_tokens: field(|u| u.candidates_token_count),
            cache_read_tokens: field(|u| u.cached_content_token_count),
            cache_write_tokens: 0,
            provider_meta: None,
        })
    }
}

#[async_trait]
impl LlmClient for GeminiClient {
    async fn chat(&self, api_key: &str, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        chat_with_effort_fallback(req.effort, |effort| self.chat_once(api_key, req, effort)).await
    }

    async fn list_models(&self, api_key: &str) -> Result<Vec<String>, LlmError> {
        let url = format!("{}/v1beta/models", self.base_url);
        let request = self
            .http
            .get(url)
            .header("x-goog-api-key", key_header(api_key)?);
        let payload: GeminiModels =
            decode(send(request, false, request_timeout(None)).await?).await?;
        Ok(normalize_model_ids(
            payload
                .models
                .into_iter()
                .filter(|m| {
                    m.supported_generation_methods
                        .as_ref()
                        .is_none_or(|methods| {
                            methods.iter().any(|method| method == "generateContent")
                        })
                })
                .map(|m| {
                    m.name
                        .strip_prefix("models/")
                        .unwrap_or(&m.name)
                        .to_string()
                }),
        ))
    }
}

/// Conversation as Gemini `contents`. Tool results travel in a `user` turn, and adjacent turns
/// with the same role are merged into one, `functionResponse` parts first, then the text.
fn wire_contents(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<(&str, Vec<Value>)> = Vec::new();
    for message in messages {
        let (role, parts) = match message {
            Message::User { parts } => (
                "user",
                parts
                    .iter()
                    .map(|UserPart::Text(t)| json!({"text": t}))
                    .collect(),
            ),
            Message::Assistant { parts } => ("model", model_parts(parts)),
            Message::Tool { results } => (
                "user",
                results
                    .iter()
                    .map(|r| {
                        json!({"functionResponse": {
                            "name": r.tool_name,
                            "response": {"result": r.content, "isError": r.is_error},
                        }})
                    })
                    .collect(),
            ),
        };
        match out.last_mut() {
            Some((last_role, last_parts)) if *last_role == role => {
                last_parts.extend(parts);
                // Stable: keeps the order within the responses and within the text.
                last_parts.sort_by_key(|p| p.get("functionResponse").is_none());
            }
            _ => out.push((role, parts)),
        }
    }
    out.into_iter()
        .map(|(role, parts)| json!({"role": role, "parts": parts}))
        .collect()
}

fn model_parts(parts: &[AssistantPart]) -> Vec<Value> {
    parts
        .iter()
        // The API rejects empty text parts and ignores opaque per-turn provider metadata.
        .filter_map(|p| match p {
            AssistantPart::Text(t) if t.is_empty() => None,
            AssistantPart::Text(t) => Some(json!({"text": t})),
            AssistantPart::ToolCall(c) => {
                let mut part = json!({"functionCall": {"name": c.name, "args": c.arguments}});
                // Thinking models reject a replayed call that lost its signature.
                if let Some(signature) = &c.provider_meta {
                    part["thoughtSignature"] = signature.as_str().into();
                }
                Some(part)
            }
            AssistantPart::ProviderMeta(_) => None,
        })
        .collect()
}

fn stop_reason(finish_reason: Option<&str>) -> StopReason {
    match finish_reason {
        Some("MAX_TOKENS") => StopReason::MaxTokens,
        Some("STOP") => StopReason::EndTurn,
        _ => StopReason::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::{thinking_config, thinking_level};
    use crate::ReasoningEffort;
    use serde_json::json;

    #[test]
    fn thinking_level_matches_gemini_3_and_later() {
        for model in [
            "gemini-3-flash-preview",
            "gemini-3-pro-preview",
            "gemini-3.1-pro-preview",
            "gemini-4-pro",
            "gemini-10-pro",
        ] {
            assert!(thinking_level(model), "{model} should use thinkingLevel");
        }

        for model in ["gemini-2.5-flash", "gemini-flash-latest", "m-1"] {
            assert!(!thinking_level(model), "{model} should use thinkingBudget");
        }
    }

    #[test]
    fn thinking_config_maps_levels_and_budget_reserves() {
        assert_eq!(
            thinking_config("gemini-3-flash-preview", ReasoningEffort::Low),
            json!({"thinkingLevel": "LOW"})
        );
        assert_eq!(
            thinking_config("gemini-3-flash-preview", ReasoningEffort::Medium),
            json!({"thinkingLevel": "MEDIUM"})
        );
        assert_eq!(
            thinking_config("gemini-3-pro-preview", ReasoningEffort::Medium),
            json!({"thinkingLevel": "HIGH"})
        );
        assert_eq!(
            thinking_config("gemini-3.1-pro-preview", ReasoningEffort::High),
            json!({"thinkingLevel": "HIGH"})
        );
        assert_eq!(
            thinking_config("gemini-2.5-flash", ReasoningEffort::High),
            json!({"thinkingBudget": 23_000})
        );
    }
}
