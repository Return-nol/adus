use std::time::Duration;

use adus_llm::{
    AssistantPart, ChatRequest, ChatResponse, LlmError, Message, ProviderKind, ReasoningEffort,
    StopReason, ToolCall, ToolDef, ToolResult, UserPart, client_for, default_base_url,
};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "sk-secret-key";

fn request() -> ChatRequest {
    ChatRequest::text("m-1", "be brief", "hello", 321)
}

async fn chat(kind: ProviderKind, server: &MockServer) -> Result<ChatResponse, LlmError> {
    chat_with(kind, server, &request()).await
}

async fn chat_with(
    kind: ProviderKind,
    server: &MockServer,
    req: &ChatRequest,
) -> Result<ChatResponse, LlmError> {
    client_for(kind, Some(&server.uri()), reqwest::Client::new())
        .chat(KEY, req)
        .await
}

/// Body JSON of the single request the mock server received.
async fn sent_body(server: &MockServer) -> Value {
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    requests[0].body_json().unwrap()
}

fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read_file".into(),
            description: "Read a file".into(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        },
        ToolDef {
            name: "grep".into(),
            description: "Search files".into(),
            parameters: json!({"type": "object", "properties": {"pattern": {"type": "string"}}}),
        },
    ]
}

fn with_tools() -> ChatRequest {
    let mut req = request();
    req.tools = tools();
    req
}

/// Turn two: the model asked for two tools and both results are back (the second failed).
fn turn_two() -> ChatRequest {
    let mut req = with_tools();
    req.messages.push(Message::Assistant {
        parts: vec![
            AssistantPart::Text("checking".into()),
            AssistantPart::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "a.rs"}),
                provider_meta: Some("sig-a".into()),
            }),
            AssistantPart::ToolCall(ToolCall {
                id: "call_2".into(),
                name: "grep".into(),
                arguments: json!({"pattern": "todo"}),
                provider_meta: None,
            }),
        ],
    });
    req.messages.push(Message::Tool {
        results: vec![
            ToolResult {
                call_id: "call_1".into(),
                tool_name: "read_file".into(),
                content: "fn main() {}".into(),
                is_error: false,
            },
            ToolResult {
                call_id: "call_2".into(),
                tool_name: "grep".into(),
                content: "bad pattern".into(),
                is_error: true,
            },
        ],
    });
    req
}

/// `turn_two` plus a user nudge right after the tool results.
fn turn_two_with_nudge() -> ChatRequest {
    let mut req = turn_two();
    req.messages.push(Message::User {
        parts: vec![UserPart::Text("please submit".into())],
    });
    req
}

/// Mock yang menjawab `response` untuk POST ke `route`, tanpa pencocokan lain.
async fn serve(route: &str, response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

const OPENAI_PATH: &str = "/chat/completions";
const ANTHROPIC_PATH: &str = "/v1/messages";
const GEMINI_PATH: &str = "/v1beta/models/m-1:generateContent";

/// (provider, route) untuk tes status yang sama di semua provider.
const ALL: [(ProviderKind, &str); 4] = [
    (ProviderKind::Openai, OPENAI_PATH),
    (ProviderKind::OpenaiCompatible, OPENAI_PATH),
    (ProviderKind::Anthropic, ANTHROPIC_PATH),
    (ProviderKind::Gemini, GEMINI_PATH),
];

// ---------- OpenAI ----------

#[tokio::test]
async fn openai_parses_text_and_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(OPENAI_PATH))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .and(body_partial_json(json!({
            "model": "m-1",
            "max_completion_tokens": 321,
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hello"},
            ],
        })))
        .respond_with(ok(json!({
            "choices": [{
                "message": {"role": "assistant", "content": "hi there"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7},
        })))
        .expect(1)
        .mount(&server)
        .await;

    let got = chat(ProviderKind::Openai, &server).await.unwrap();
    assert_eq!(
        got,
        ChatResponse {
            text: "hi there".into(),
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            input_tokens: 11,
            output_tokens: 7,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        }
    );
}

#[tokio::test]
async fn openai_sends_max_completion_tokens_only() {
    let server = serve(
        OPENAI_PATH,
        ok(json!({"choices": [{"message": {"content": "x"}}]})),
    )
    .await;
    chat(ProviderKind::Openai, &server).await.unwrap();
    let body: Value = server.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(body["max_completion_tokens"], 321);
    assert!(body.get("max_tokens").is_none());
}

#[tokio::test]
async fn compatible_sends_max_tokens_only() {
    let server = serve(
        OPENAI_PATH,
        ok(json!({"choices": [{"message": {"content": "x"}}]})),
    )
    .await;
    chat(ProviderKind::OpenaiCompatible, &server).await.unwrap();
    let body: Value = server.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(body["max_tokens"], 321);
    assert!(body.get("max_completion_tokens").is_none());
}

#[tokio::test]
async fn openai_missing_usage_is_zero() {
    let server = serve(
        OPENAI_PATH,
        ok(json!({"choices": [{"message": {"content": "x"}}]})),
    )
    .await;
    let got = chat(ProviderKind::Openai, &server).await.unwrap();
    assert_eq!((got.input_tokens, got.output_tokens), (0, 0));
}

#[tokio::test]
async fn openai_empty_content_is_invalid() {
    let server = serve(
        OPENAI_PATH,
        ok(json!({"choices": [{"message": {"content": null}}]})),
    )
    .await;
    assert_eq!(
        chat(ProviderKind::Openai, &server).await.unwrap_err(),
        LlmError::Invalid("empty response".into())
    );
}

#[tokio::test]
async fn compatible_uses_custom_base_url() {
    // Base URL dengan path sendiri dan '/' di akhir.
    let server = serve(
        "/custom/v9/chat/completions",
        ok(json!({"choices": [{"message": {"content": "from custom"}}]})),
    )
    .await;
    let base = format!("{}/custom/v9/", server.uri());
    let got = client_for(
        ProviderKind::OpenaiCompatible,
        Some(&base),
        reqwest::Client::new(),
    )
    .chat(KEY, &request())
    .await
    .unwrap();
    assert_eq!(got.text, "from custom");
}

#[tokio::test]
async fn compatible_without_base_url_is_invalid() {
    let err = client_for(ProviderKind::OpenaiCompatible, None, reqwest::Client::new())
        .chat(KEY, &request())
        .await
        .unwrap_err();
    assert_eq!(
        err,
        LlmError::Invalid("base_url required for openai_compatible".into())
    );
}

#[test]
fn default_base_urls() {
    assert_eq!(
        default_base_url(ProviderKind::Openai),
        Some("https://api.openai.com/v1")
    );
    assert_eq!(
        default_base_url(ProviderKind::Anthropic),
        Some("https://api.anthropic.com")
    );
    assert_eq!(
        default_base_url(ProviderKind::Gemini),
        Some("https://generativelanguage.googleapis.com")
    );
    assert_eq!(default_base_url(ProviderKind::OpenaiCompatible), None);
}

// ---------- Anthropic ----------

#[tokio::test]
async fn anthropic_parses_text_and_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ANTHROPIC_PATH))
        .and(header("x-api-key", KEY))
        .and(header("anthropic-version", "2023-06-01"))
        .and(body_partial_json(json!({
            "model": "m-1",
            "system": [{
                "type": "text",
                "text": "be brief",
                "cache_control": {"type": "ephemeral"},
            }],
            "max_tokens": 321,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}],
        })))
        .respond_with(ok(json!({
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "part one, "},
                {"type": "text", "text": "part two"},
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 9},
        })))
        .expect(1)
        .mount(&server)
        .await;

    let got = chat(ProviderKind::Anthropic, &server).await.unwrap();
    assert_eq!(
        got,
        ChatResponse {
            text: "part one, part two".into(),
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            input_tokens: 5,
            output_tokens: 9,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        }
    );
}

#[tokio::test]
async fn anthropic_no_text_blocks_is_invalid() {
    let server = serve(
        ANTHROPIC_PATH,
        ok(json!({"content": [{"type": "thinking", "thinking": "hmm"}]})),
    )
    .await;
    assert_eq!(
        chat(ProviderKind::Anthropic, &server).await.unwrap_err(),
        LlmError::Invalid("empty response".into())
    );
}

// ---------- Gemini ----------

#[tokio::test]
async fn gemini_parses_text_and_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(GEMINI_PATH))
        .and(header("x-goog-api-key", KEY))
        .and(body_partial_json(json!({
            "systemInstruction": {"parts": [{"text": "be brief"}]},
            "contents": [{"role": "user", "parts": [{"text": "hello"}]}],
            "generationConfig": {"maxOutputTokens": 321},
        })))
        .respond_with(ok(json!({
            "candidates": [{
                "content": {"parts": [{"text": "a"}, {"text": "b"}]},
                "finishReason": "STOP",
            }],
            "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4},
        })))
        .expect(1)
        .mount(&server)
        .await;

    let got = chat(ProviderKind::Gemini, &server).await.unwrap();
    assert_eq!(
        got,
        ChatResponse {
            text: "ab".into(),
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            input_tokens: 3,
            output_tokens: 4,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        }
    );
}

#[tokio::test]
async fn gemini_block_reason_is_invalid() {
    let server = serve(
        GEMINI_PATH,
        ok(json!({"promptFeedback": {"blockReason": "SAFETY"}})),
    )
    .await;
    assert_eq!(
        chat(ProviderKind::Gemini, &server).await.unwrap_err(),
        LlmError::Invalid("blocked: SAFETY".into())
    );
}

#[tokio::test]
async fn gemini_no_candidates_is_invalid() {
    let server = serve(GEMINI_PATH, ok(json!({"candidates": []}))).await;
    assert_eq!(
        chat(ProviderKind::Gemini, &server).await.unwrap_err(),
        LlmError::Invalid("empty response".into())
    );
}

// ---------- Status mapping, sama untuk semua provider ----------

#[tokio::test]
async fn status_429_with_retry_after_seconds() {
    for (kind, route) in ALL {
        let server = serve(
            route,
            ResponseTemplate::new(429).insert_header("retry-after", "17"),
        )
        .await;
        assert_eq!(
            chat(kind, &server).await.unwrap_err(),
            LlmError::RateLimited {
                retry_after: Some(Duration::from_secs(17))
            },
            "{kind:?}"
        );
    }
}

#[tokio::test]
async fn status_429_without_or_with_date_retry_after_is_none() {
    for retry_after in [None, Some("Wed, 21 Oct 2026 07:28:00 GMT")] {
        let mut response = ResponseTemplate::new(429);
        if let Some(v) = retry_after {
            response = response.insert_header("retry-after", v);
        }
        let server = serve(OPENAI_PATH, response).await;
        assert_eq!(
            chat(ProviderKind::Openai, &server).await.unwrap_err(),
            LlmError::RateLimited { retry_after: None }
        );
    }
}

#[tokio::test]
async fn status_401_and_403_are_unauthorized() {
    for (kind, route) in ALL {
        for status in [401, 403] {
            let server = serve(route, ResponseTemplate::new(status)).await;
            assert_eq!(
                chat(kind, &server).await.unwrap_err(),
                LlmError::Unauthorized,
                "{kind:?} {status}"
            );
        }
    }
}

#[tokio::test]
async fn status_5xx_is_transient() {
    for (kind, route) in ALL {
        for status in [500, 503] {
            let server = serve(route, ResponseTemplate::new(status)).await;
            let err = chat(kind, &server).await.unwrap_err();
            assert!(matches!(err, LlmError::Transient(_)), "{kind:?}: {err:?}");
        }
    }
}

#[tokio::test]
async fn connection_failure_is_transient_and_hides_url_and_key() {
    // Port 1: tidak ada yang mendengarkan, koneksi ditolak (stabil, tidak ada risiko port dipakai ulang).
    let uri = "http://127.0.0.1:1".to_string();
    let err = client_for(ProviderKind::Openai, Some(&uri), reqwest::Client::new())
        .chat(KEY, &request())
        .await
        .unwrap_err();
    let LlmError::Transient(msg) = &err else {
        panic!("expected Transient, got {err:?}");
    };
    assert!(!msg.contains(KEY) && !msg.contains(&uri), "{msg}");
}

#[tokio::test]
async fn status_400_404_422_are_invalid_with_provider_message() {
    for (kind, route) in ALL {
        for status in [400, 404, 422] {
            let server = serve(
                route,
                ResponseTemplate::new(status)
                    .set_body_json(json!({"error": {"message": "model 'm-1' not found"}})),
            )
            .await;
            let err = chat(kind, &server).await.unwrap_err();
            let LlmError::Invalid(msg) = &err else {
                panic!("{kind:?} {status}: expected Invalid, got {err:?}");
            };
            assert!(msg.contains(&status.to_string()), "{msg}");
            assert!(msg.contains("model 'm-1' not found"), "{msg}");
            assert!(!msg.contains(KEY), "{msg}");
        }
    }
}

#[tokio::test]
async fn invalid_message_is_truncated_and_survives_non_json_body() {
    let long = "x".repeat(500);
    let server = serve(
        OPENAI_PATH,
        ResponseTemplate::new(400).set_body_json(json!({"error": {"message": long}})),
    )
    .await;
    let LlmError::Invalid(msg) = chat(ProviderKind::Openai, &server).await.unwrap_err() else {
        panic!("expected Invalid");
    };
    assert_eq!(msg.matches('x').count(), 200);

    let server = serve(
        OPENAI_PATH,
        ResponseTemplate::new(400).set_body_string("<html>"),
    )
    .await;
    assert_eq!(
        chat(ProviderKind::Openai, &server).await.unwrap_err(),
        LlmError::Invalid("provider status 400 Bad Request".into())
    );
}

#[tokio::test]
async fn other_status_is_invalid_without_body() {
    let server = serve(OPENAI_PATH, ResponseTemplate::new(418)).await;
    let err = chat(ProviderKind::Openai, &server).await.unwrap_err();
    assert!(matches!(err, LlmError::Invalid(_)), "{err:?}");
}

#[tokio::test]
async fn gemini_invalid_api_key_is_unauthorized() {
    let by_reason = json!({"error": {
        "code": 400,
        "message": "whatever",
        "status": "INVALID_ARGUMENT",
        "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "API_KEY_INVALID"}],
    }});
    let by_status = json!({"error": {
        "code": 400,
        "message": "API key not valid. Please pass a valid API key.",
        "status": "INVALID_ARGUMENT",
    }});
    for body in [by_reason, by_status] {
        let server = serve(GEMINI_PATH, ResponseTemplate::new(400).set_body_json(body)).await;
        assert_eq!(
            chat(ProviderKind::Gemini, &server).await.unwrap_err(),
            LlmError::Unauthorized
        );
    }
}

#[tokio::test]
async fn gemini_other_invalid_argument_stays_invalid() {
    let server = serve(
        GEMINI_PATH,
        ResponseTemplate::new(400).set_body_json(
            json!({"error": {"message": "Unknown field", "status": "INVALID_ARGUMENT"}}),
        ),
    )
    .await;
    let err = chat(ProviderKind::Gemini, &server).await.unwrap_err();
    assert!(matches!(err, LlmError::Invalid(_)), "{err:?}");
}

#[tokio::test]
async fn retry_after_is_clamped_to_one_hour() {
    let server = serve(
        OPENAI_PATH,
        ResponseTemplate::new(429).insert_header("retry-after", u64::MAX.to_string().as_str()),
    )
    .await;
    assert_eq!(
        chat(ProviderKind::Openai, &server).await.unwrap_err(),
        LlmError::RateLimited {
            retry_after: Some(Duration::from_secs(3600))
        }
    );
}

#[tokio::test]
async fn malformed_base_url_is_invalid_not_transient() {
    let err = client_for(
        ProviderKind::Openai,
        Some("not a url"),
        reqwest::Client::new(),
    )
    .chat(KEY, &request())
    .await
    .unwrap_err();
    assert_eq!(err, LlmError::Invalid("invalid request".into()));
}

#[tokio::test]
async fn truncated_200_body_is_transient_but_bad_json_is_invalid() {
    // Server mentah: janji 100 byte, kirim 4 byte, lalu tutup koneksi.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let response = [
            "HTTP/1.1 200 OK",
            "content-type: application/json",
            "content-length: 100",
            "",
            "{\"a\"",
        ]
        .join(
            "
",
        );
        let _ = stream.write_all(response.as_bytes());
    });
    let err = client_for(ProviderKind::Openai, Some(&uri), reqwest::Client::new())
        .chat(KEY, &request())
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::Transient(_)), "{err:?}");

    let server = serve(
        OPENAI_PATH,
        ResponseTemplate::new(200).set_body_string("not json"),
    )
    .await;
    assert_eq!(
        chat(ProviderKind::Openai, &server).await.unwrap_err(),
        LlmError::Invalid("malformed response".into())
    );
}

#[tokio::test]
async fn multibyte_message_is_truncated_on_char_boundary() {
    let long = "é".repeat(300);
    let server = serve(
        OPENAI_PATH,
        ResponseTemplate::new(400).set_body_json(json!({"error": {"message": long}})),
    )
    .await;
    let LlmError::Invalid(msg) = chat(ProviderKind::Openai, &server).await.unwrap_err() else {
        panic!("expected Invalid");
    };
    assert_eq!(msg.matches('é').count(), 200);
}

// ---------- Tool calling: OpenAI ----------

#[tokio::test]
async fn openai_sends_tools_and_parses_tool_calls() {
    let server = serve(
        OPENAI_PATH,
        ok(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":\"b.rs\"}"},
                    }],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {
                "prompt_tokens": 120,
                "completion_tokens": 8,
                "prompt_tokens_details": {"cached_tokens": 100},
            },
        })),
    )
    .await;

    let got = chat_with(ProviderKind::Openai, &server, &with_tools())
        .await
        .unwrap();
    assert_eq!(
        got,
        ChatResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_9".into(),
                name: "read_file".into(),
                arguments: json!({"path": "b.rs"}),
                provider_meta: None,
            }],
            stop_reason: StopReason::ToolUse,
            input_tokens: 120,
            output_tokens: 8,
            cache_read_tokens: 100,
            cache_write_tokens: 0,
        }
    );
    let body = sent_body(&server).await;
    assert_eq!(
        body["tools"],
        json!([
            {"type": "function", "function": {
                "name": "read_file",
                "description": "Read a file",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
            }},
            {"type": "function", "function": {
                "name": "grep",
                "description": "Search files",
                "parameters": {"type": "object", "properties": {"pattern": {"type": "string"}}},
            }},
        ])
    );
    assert_eq!(body["tool_choice"], "auto");
}

/// Some compatible servers (older Ollama, LocalAI) send tool calls without an `id`, or with an
/// empty one; the client makes one up that is unique in the conversation.
#[tokio::test]
async fn compatible_tool_calls_without_id_get_unique_synthetic_ids() {
    let server = serve(
        OPENAI_PATH,
        ok(json!({"choices": [{
            "message": {"content": null, "tool_calls": [
                {"type": "function",
                 "function": {"name": "grep", "arguments": "{\"pattern\":\"x\"}"}},
                {"id": "", "type": "function",
                 "function": {"name": "read_file", "arguments": "{\"path\":\"d.rs\"}"}},
                {"id": "call_kept", "type": "function",
                 "function": {"name": "read_file", "arguments": "{\"path\":\"e.rs\"}"}},
            ]},
            "finish_reason": "tool_calls",
        }]})),
    )
    .await;
    // `turn_two` already holds two tool calls, so new ids start at call_2_*.
    let got = chat_with(ProviderKind::OpenaiCompatible, &server, &turn_two())
        .await
        .unwrap();
    let ids: Vec<&str> = got.tool_calls.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["call_2_0", "call_2_1", "call_kept"]);
    assert_eq!(got.tool_calls[0].arguments, json!({"pattern": "x"}));
    assert_eq!(got.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn openai_without_tools_omits_tools_and_tool_choice() {
    let server = serve(
        OPENAI_PATH,
        ok(json!({"choices": [{"message": {"content": "x"}}]})),
    )
    .await;
    chat(ProviderKind::Openai, &server).await.unwrap();
    let body = sent_body(&server).await;
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
}

#[tokio::test]
async fn openai_serializes_tool_calls_and_tool_role_on_turn_two() {
    let server = serve(
        OPENAI_PATH,
        ok(json!({"choices": [{"message": {"content": "done"}, "finish_reason": "stop"}]})),
    )
    .await;
    chat_with(ProviderKind::Openai, &server, &turn_two())
        .await
        .unwrap();
    assert_eq!(
        sent_body(&server).await["messages"],
        json!([
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "checking", "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}},
                {"id": "call_2", "type": "function",
                 "function": {"name": "grep", "arguments": "{\"pattern\":\"todo\"}"}},
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "fn main() {}"},
            {"role": "tool", "tool_call_id": "call_2", "content": "bad pattern"},
        ])
    );
}

#[tokio::test]
async fn openai_assistant_with_only_tool_calls_sends_null_content() {
    let server = serve(
        OPENAI_PATH,
        ok(json!({"choices": [{"message": {"content": "done"}}]})),
    )
    .await;
    let mut req = turn_two();
    if let Message::Assistant { parts } = &mut req.messages[1] {
        parts.remove(0);
    }
    chat_with(ProviderKind::Openai, &server, &req)
        .await
        .unwrap();
    let body = sent_body(&server).await;
    assert_eq!(body["messages"][2]["content"], Value::Null);
    assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call_1");
}

#[tokio::test]
async fn openai_malformed_tool_arguments_are_wrapped_not_fatal() {
    let server = serve(
        OPENAI_PATH,
        ok(json!({"choices": [{
            "message": {"content": null, "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "grep", "arguments": "{\"pattern\": "},
            }]},
            "finish_reason": "tool_calls",
        }]})),
    )
    .await;
    let got = chat_with(ProviderKind::Openai, &server, &with_tools())
        .await
        .unwrap();
    let args = &got.tool_calls[0].arguments;
    assert_eq!(args["raw"], "{\"pattern\": ");
    assert!(
        args["__raw_parse_error"]
            .as_str()
            .is_some_and(|e| !e.is_empty())
    );
}

#[tokio::test]
async fn compatible_with_tools_still_sends_max_tokens_only() {
    let server = serve(
        OPENAI_PATH,
        ok(json!({"choices": [{"message": {"content": "x"}}]})),
    )
    .await;
    chat_with(ProviderKind::OpenaiCompatible, &server, &with_tools())
        .await
        .unwrap();
    let body = sent_body(&server).await;
    assert_eq!(body["max_tokens"], 321);
    assert!(body.get("max_completion_tokens").is_none());
    assert_eq!(body["tools"][0]["function"]["name"], "read_file");
}

#[tokio::test]
async fn compatible_model_without_tool_support_is_tools_unsupported() {
    // OpenAI-style object error and Ollama-style string error.
    for body in [
        json!({"error": {"message": "This model does not support tools"}}),
        json!({"error": "registry.ollama.ai/library/tiny:latest does not support tools"}),
    ] {
        let server = serve(OPENAI_PATH, ResponseTemplate::new(400).set_body_json(body)).await;
        let err = chat_with(ProviderKind::OpenaiCompatible, &server, &with_tools())
            .await
            .unwrap_err();
        let LlmError::ToolsUnsupported(msg) = &err else {
            panic!("expected ToolsUnsupported, got {err:?}");
        };
        assert!(msg.contains("does not support tools"), "{msg}");
        assert!(!msg.contains(KEY), "{msg}");
    }
}

#[tokio::test]
async fn tools_marker_without_tools_in_request_stays_invalid() {
    let server = serve(
        OPENAI_PATH,
        ResponseTemplate::new(400)
            .set_body_json(json!({"error": {"message": "model does not support tools"}})),
    )
    .await;
    let err = chat(ProviderKind::OpenaiCompatible, &server)
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::Invalid(_)), "{err:?}");
}

#[tokio::test]
async fn unknown_tools_field_is_tools_unsupported() {
    let server = serve(
        OPENAI_PATH,
        ResponseTemplate::new(422)
            .set_body_json(json!({"error": {"message": "Unknown field: tools"}})),
    )
    .await;
    let err = chat_with(ProviderKind::OpenaiCompatible, &server, &with_tools())
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::ToolsUnsupported(_)), "{err:?}");
}

#[tokio::test]
async fn compatible_maximum_context_length_is_context_overflow() {
    let server = serve(
        OPENAI_PATH,
        ResponseTemplate::new(400).set_body_json(json!({"error": {
            "message": "This model's maximum context length is 8192 tokens.",
            "code": "context_length_exceeded",
        }})),
    )
    .await;
    let err = chat(ProviderKind::OpenaiCompatible, &server)
        .await
        .unwrap_err();
    let LlmError::ContextOverflow(msg) = &err else {
        panic!("expected ContextOverflow, got {err:?}");
    };
    assert!(msg.contains("maximum context length"), "{msg}");
}

#[tokio::test]
async fn context_overflow_markers_map_for_every_provider() {
    for (kind, route) in ALL {
        for (status, message) in [
            (400, "prompt is too long: 210000 tokens > 200000 maximum"),
            (
                400,
                "The input token count exceeds the maximum number of tokens allowed",
            ),
            (422, "Input is too long for requested model"),
        ] {
            let server = serve(
                route,
                ResponseTemplate::new(status).set_body_json(json!({"error": {"message": message}})),
            )
            .await;
            let err = chat(kind, &server).await.unwrap_err();
            assert!(
                matches!(err, LlmError::ContextOverflow(_)),
                "{kind:?} {status} {message}: {err:?}"
            );
        }
    }
}

#[tokio::test]
async fn status_413_is_context_overflow() {
    for (kind, route) in ALL {
        let server = serve(
            route,
            ResponseTemplate::new(413)
                .set_body_json(json!({"error": {"message": "Request exceeds the maximum size"}})),
        )
        .await;
        let err = chat(kind, &server).await.unwrap_err();
        let LlmError::ContextOverflow(msg) = &err else {
            panic!("{kind:?}: expected ContextOverflow, got {err:?}");
        };
        assert!(msg.contains("413"), "{msg}");
    }
    // Without a body too.
    let server = serve(OPENAI_PATH, ResponseTemplate::new(413)).await;
    let err = chat(ProviderKind::Openai, &server).await.unwrap_err();
    assert!(matches!(err, LlmError::ContextOverflow(_)), "{err:?}");
}

#[test]
fn new_llm_error_display_text() {
    assert_eq!(
        LlmError::ToolsUnsupported("x".into()).to_string(),
        "tools not supported: x"
    );
    assert_eq!(
        LlmError::ContextOverflow("y".into()).to_string(),
        "context window exceeded: y"
    );
}

#[tokio::test]
async fn length_finish_maps_to_max_tokens_for_every_provider() {
    let cases = [
        (
            ProviderKind::Openai,
            OPENAI_PATH,
            json!({"choices": [{"message": {"content": "cut"}, "finish_reason": "length"}]}),
        ),
        (
            ProviderKind::Anthropic,
            ANTHROPIC_PATH,
            json!({"content": [{"type": "text", "text": "cut"}], "stop_reason": "max_tokens"}),
        ),
        (
            ProviderKind::Gemini,
            GEMINI_PATH,
            json!({"candidates": [{
                "content": {"parts": [{"text": "cut"}]},
                "finishReason": "MAX_TOKENS",
            }]}),
        ),
    ];
    for (kind, route, response) in cases {
        let server = serve(route, ok(response)).await;
        let got = chat(kind, &server).await.unwrap();
        assert_eq!(got.stop_reason, StopReason::MaxTokens, "{kind:?}");
    }
}

// ---------- Tool calling: Anthropic ----------

#[tokio::test]
async fn anthropic_sends_cache_control_on_system_last_tool_and_checkpoint() {
    let server = serve(
        ANTHROPIC_PATH,
        ok(json!({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn"})),
    )
    .await;
    chat_with(ProviderKind::Anthropic, &server, &turn_two())
        .await
        .unwrap();
    let ephemeral = json!({"type": "ephemeral"});
    assert_eq!(
        sent_body(&server).await,
        json!({
            "model": "m-1",
            "max_tokens": 321,
            "system": [{"type": "text", "text": "be brief", "cache_control": ephemeral}],
            "tools": [
                {
                    "name": "read_file",
                    "description": "Read a file",
                    "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
                },
                {
                    "name": "grep",
                    "description": "Search files",
                    "input_schema": {"type": "object", "properties": {"pattern": {"type": "string"}}},
                    "cache_control": ephemeral,
                },
            ],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "call_1", "name": "read_file", "input": {"path": "a.rs"}},
                    {"type": "tool_use", "id": "call_2", "name": "grep", "input": {"pattern": "todo"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "fn main() {}", "is_error": false},
                    {"type": "tool_result", "tool_use_id": "call_2", "content": "bad pattern", "is_error": true,
                     "cache_control": ephemeral},
                ]},
            ],
        })
    );
}

#[tokio::test]
async fn anthropic_merges_tool_results_and_following_user_text_into_one_user_turn() {
    let server = serve(
        ANTHROPIC_PATH,
        ok(json!({"content": [{"type": "text", "text": "done"}]})),
    )
    .await;
    chat_with(ProviderKind::Anthropic, &server, &turn_two_with_nudge())
        .await
        .unwrap();
    let messages = &sent_body(&server).await["messages"];
    assert_eq!(messages.as_array().map(Vec::len), Some(3));
    assert_eq!(
        messages[2],
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "call_1", "content": "fn main() {}", "is_error": false},
            {"type": "tool_result", "tool_use_id": "call_2", "content": "bad pattern", "is_error": true},
            {"type": "text", "text": "please submit", "cache_control": {"type": "ephemeral"}},
        ]})
    );
}

#[tokio::test]
async fn anthropic_empty_system_and_no_tools_are_omitted() {
    let server = serve(
        ANTHROPIC_PATH,
        ok(json!({"content": [{"type": "text", "text": "pong"}]})),
    )
    .await;
    let req = ChatRequest::text("m-1", "", "ping", 5);
    chat_with(ProviderKind::Anthropic, &server, &req)
        .await
        .unwrap();
    assert_eq!(
        sent_body(&server).await,
        json!({
            "model": "m-1",
            "max_tokens": 5,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "ping"}]}],
        })
    );
}

#[tokio::test]
async fn anthropic_parses_tool_use_and_cache_usage() {
    let server = serve(
        ANTHROPIC_PATH,
        ok(json!({
            "content": [
                {"type": "text", "text": "Let me look."},
                {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "c.rs"}},
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 7,
                "cache_read_input_tokens": 100,
                "cache_creation_input_tokens": 50,
            },
        })),
    )
    .await;
    let got = chat_with(ProviderKind::Anthropic, &server, &with_tools())
        .await
        .unwrap();
    assert_eq!(
        got,
        ChatResponse {
            text: "Let me look.".into(),
            tool_calls: vec![ToolCall {
                id: "toolu_1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "c.rs"}),
                provider_meta: None,
            }],
            stop_reason: StopReason::ToolUse,
            input_tokens: 160,
            output_tokens: 7,
            cache_read_tokens: 100,
            cache_write_tokens: 50,
        }
    );
}

// ---------- Tool calling: Gemini ----------

#[tokio::test]
async fn gemini_sends_function_declarations_and_round_trips_thought_signature() {
    let server = serve(
        GEMINI_PATH,
        ok(json!({"candidates": [{"content": {"parts": [{"text": "done"}]}}]})),
    )
    .await;
    chat_with(ProviderKind::Gemini, &server, &turn_two_with_nudge())
        .await
        .unwrap();
    assert_eq!(
        sent_body(&server).await,
        json!({
            "systemInstruction": {"parts": [{"text": "be brief"}]},
            "generationConfig": {"maxOutputTokens": 321},
            "tools": [{"functionDeclarations": [
                {
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
                },
                {
                    "name": "grep",
                    "description": "Search files",
                    "parameters": {"type": "object", "properties": {"pattern": {"type": "string"}}},
                },
            ]}],
            "toolConfig": {"functionCallingConfig": {"mode": "AUTO"}},
            "contents": [
                {"role": "user", "parts": [{"text": "hello"}]},
                {"role": "model", "parts": [
                    {"text": "checking"},
                    {"functionCall": {"name": "read_file", "args": {"path": "a.rs"}},
                     "thoughtSignature": "sig-a"},
                    {"functionCall": {"name": "grep", "args": {"pattern": "todo"}}},
                ]},
                {"role": "user", "parts": [
                    {"functionResponse": {"name": "read_file",
                        "response": {"result": "fn main() {}", "isError": false}}},
                    {"functionResponse": {"name": "grep",
                        "response": {"result": "bad pattern", "isError": true}}},
                    {"text": "please submit"},
                ]},
            ],
        })
    );
}

#[tokio::test]
async fn gemini_without_tools_omits_tools_and_tool_config() {
    let server = serve(
        GEMINI_PATH,
        ok(json!({"candidates": [{"content": {"parts": [{"text": "x"}]}}]})),
    )
    .await;
    chat(ProviderKind::Gemini, &server).await.unwrap();
    let body = sent_body(&server).await;
    assert!(body.get("tools").is_none());
    assert!(body.get("toolConfig").is_none());
}

#[tokio::test]
async fn gemini_assigns_unique_synthetic_ids_and_keeps_thought_signature() {
    let server = serve(
        GEMINI_PATH,
        ok(json!({
            "candidates": [{
                "content": {"role": "model", "parts": [
                    {"functionCall": {"name": "grep", "args": {"pattern": "x"}},
                     "thoughtSignature": "sig-b"},
                    {"functionCall": {"name": "read_file", "args": {"path": "d.rs"}}},
                ]},
                "finishReason": "STOP",
            }],
            "usageMetadata": {
                "promptTokenCount": 300,
                "candidatesTokenCount": 12,
                "cachedContentTokenCount": 256,
            },
        })),
    )
    .await;
    // `turn_two` already holds two tool calls, so new ids start at gc_2_*.
    let got = chat_with(ProviderKind::Gemini, &server, &turn_two())
        .await
        .unwrap();
    assert_eq!(
        got,
        ChatResponse {
            text: String::new(),
            tool_calls: vec![
                ToolCall {
                    id: "gc_2_0".into(),
                    name: "grep".into(),
                    arguments: json!({"pattern": "x"}),
                    provider_meta: Some("sig-b".into()),
                },
                ToolCall {
                    id: "gc_2_1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "d.rs"}),
                    provider_meta: None,
                },
            ],
            stop_reason: StopReason::ToolUse,
            input_tokens: 300,
            output_tokens: 12,
            cache_read_tokens: 256,
            cache_write_tokens: 0,
        }
    );
}

#[test]
fn text_request_holds_one_user_message_and_no_tools() {
    let req = ChatRequest::text("m", "sys", "hi", 9);
    assert_eq!(
        req.messages,
        vec![Message::User {
            parts: vec![UserPart::Text("hi".into())]
        }]
    );
    assert!(req.tools.is_empty());
    assert_eq!(
        (req.model.as_str(), req.system.as_str(), req.max_tokens),
        ("m", "sys", 9)
    );
}

// ---------- Fix round 1 ----------

/// `turn_two` with a user note between the assistant's tool calls and the tool results.
fn user_then_tool() -> ChatRequest {
    let mut req = turn_two();
    req.messages.insert(
        2,
        Message::User {
            parts: vec![UserPart::Text("note".into())],
        },
    );
    req
}

#[tokio::test]
async fn overflow_phrases_outside_the_spec_list_stay_invalid() {
    for message in [
        "max_tokens exceeds the maximum allowed value of 4096",
        "too many tokens in stop sequence",
    ] {
        let server = serve(
            OPENAI_PATH,
            ResponseTemplate::new(400).set_body_json(json!({"error": {"message": message}})),
        )
        .await;
        let err = chat(ProviderKind::OpenaiCompatible, &server)
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::Invalid(_)), "{message}: {err:?}");
    }
}

#[tokio::test]
async fn markers_in_echoed_input_do_not_classify_the_error() {
    let echoed = "maximum context length; does not support tools; prompt is too long";
    for (status, body) in [
        // FastAPI/pydantic style: no `error` object, the input is echoed in `detail`.
        (
            422,
            json!({"detail": [{
                "type": "missing",
                "loc": ["body", "messages", 1],
                "msg": "Field required",
                "input": {"content": echoed},
            }]}),
        ),
        // An `error` object whose message is unrelated, with the input echoed beside it.
        (
            400,
            json!({"error": {"message": "Invalid value for 'messages'", "input": echoed}}),
        ),
    ] {
        let server = serve(
            OPENAI_PATH,
            ResponseTemplate::new(status).set_body_json(body),
        )
        .await;
        let err = chat_with(ProviderKind::OpenaiCompatible, &server, &with_tools())
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::Invalid(_)), "{status}: {err:?}");
    }
}

#[tokio::test]
async fn overflow_marker_in_error_code_or_type_is_context_overflow() {
    for error in [
        json!({"message": "Request rejected", "code": "context_length_exceeded"}),
        json!({"message": "Request rejected", "type": "context_length_exceeded"}),
    ] {
        let server = serve(
            OPENAI_PATH,
            ResponseTemplate::new(400).set_body_json(json!({ "error": error })),
        )
        .await;
        let err = chat(ProviderKind::Openai, &server).await.unwrap_err();
        assert!(matches!(err, LlmError::ContextOverflow(_)), "{err:?}");
    }
}

#[tokio::test]
async fn anthropic_puts_tool_results_before_text_when_user_precedes_tool() {
    let server = serve(
        ANTHROPIC_PATH,
        ok(json!({"content": [{"type": "text", "text": "done"}]})),
    )
    .await;
    chat_with(ProviderKind::Anthropic, &server, &user_then_tool())
        .await
        .unwrap();
    let messages = &sent_body(&server).await["messages"];
    assert_eq!(messages.as_array().map(Vec::len), Some(3));
    assert_eq!(
        messages[2],
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "call_1", "content": "fn main() {}", "is_error": false},
            {"type": "tool_result", "tool_use_id": "call_2", "content": "bad pattern", "is_error": true},
            {"type": "text", "text": "note", "cache_control": {"type": "ephemeral"}},
        ]})
    );
}

#[tokio::test]
async fn gemini_puts_function_responses_before_text_when_user_precedes_tool() {
    let server = serve(
        GEMINI_PATH,
        ok(json!({"candidates": [{"content": {"parts": [{"text": "done"}]}}]})),
    )
    .await;
    chat_with(ProviderKind::Gemini, &server, &user_then_tool())
        .await
        .unwrap();
    let contents = &sent_body(&server).await["contents"];
    assert_eq!(contents.as_array().map(Vec::len), Some(3));
    assert_eq!(
        contents[2],
        json!({"role": "user", "parts": [
            {"functionResponse": {"name": "read_file",
                "response": {"result": "fn main() {}", "isError": false}}},
            {"functionResponse": {"name": "grep",
                "response": {"result": "bad pattern", "isError": true}}},
            {"text": "note"},
        ]})
    );
}

#[tokio::test]
async fn gemini_drops_empty_assistant_text_parts() {
    let server = serve(
        GEMINI_PATH,
        ok(json!({"candidates": [{"content": {"parts": [{"text": "done"}]}}]})),
    )
    .await;
    let mut req = turn_two();
    if let Message::Assistant { parts } = &mut req.messages[1] {
        parts[0] = AssistantPart::Text(String::new());
    }
    chat_with(ProviderKind::Gemini, &server, &req)
        .await
        .unwrap();
    assert_eq!(
        sent_body(&server).await["contents"][1],
        json!({"role": "model", "parts": [
            {"functionCall": {"name": "read_file", "args": {"path": "a.rs"}},
             "thoughtSignature": "sig-a"},
            {"functionCall": {"name": "grep", "args": {"pattern": "todo"}}},
        ]})
    );
}

#[tokio::test]
async fn openai_sends_reasoning_effort_when_configured() {
    let server = serve(
        OPENAI_PATH,
        ok(json!({"choices": [{"message": {"content": "ok"}}]})),
    )
    .await;
    let req = ChatRequest::text("o3", "sys", "hi", 100).with_effort(ReasoningEffort::High);
    chat_with(ProviderKind::Openai, &server, &req)
        .await
        .unwrap();
    let body = sent_body(&server).await;
    assert_eq!(body["reasoning_effort"], "high");
    // Reasoning tokens count against the cap, so High reserves 8192 on top of the answer.
    assert_eq!(body["max_completion_tokens"], 100 + 8192);
}

#[tokio::test]
async fn retries_without_effort_when_provider_rejects_reasoning_param() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(OPENAI_PATH))
        .and(body_partial_json(json!({"reasoning_effort": "high"})))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "Unrecognized request argument supplied: reasoning_effort"}
        })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(OPENAI_PATH))
        .respond_with(ok(json!({
            "choices": [{"message": {"content": "recovered"}, "finish_reason": "stop"}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let req = ChatRequest::text("gpt-4o-mini", "sys", "hi", 100).with_effort(ReasoningEffort::High);
    let resp = chat_with(ProviderKind::Openai, &server, &req)
        .await
        .unwrap();
    assert_eq!(resp.text, "recovered");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let first: Value = requests[0].body_json().unwrap();
    let second: Value = requests[1].body_json().unwrap();
    assert_eq!(first["reasoning_effort"], "high");
    assert_eq!(first["max_completion_tokens"], 100 + 8192);
    assert!(second.get("reasoning_effort").is_none());
    assert_eq!(second["max_completion_tokens"], 100);
}

#[tokio::test]
async fn anthropic_sends_thinking_budget_and_replays_thinking_blocks_on_tool_turns() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ANTHROPIC_PATH))
        .respond_with(ok(json!({
            "content": [
                {"type": "thinking", "thinking": "check caller", "signature": "sig_1"},
                {"type": "redacted_thinking", "data": "red_1"},
                {"type": "tool_use", "id": "tu_1", "name": "read_file", "input": {"path": "a.rs"}}
            ],
            "stop_reason": "tool_use"
        })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(ANTHROPIC_PATH))
        .respond_with(ok(json!({
            "content": [{"type": "text", "text": "done"}],
            "stop_reason": "end_turn"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut req = ChatRequest::text("claude-sonnet-4-5", "sys", "hi", 8192)
        .with_effort(ReasoningEffort::Medium);
    req.tools = tools();

    let first_resp = chat_with(ProviderKind::Anthropic, &server, &req)
        .await
        .unwrap();
    assert_eq!(first_resp.tool_calls.len(), 1);
    assert_eq!(first_resp.tool_calls[0].id, "tu_1");

    req.messages.push(Message::Assistant {
        parts: vec![AssistantPart::ToolCall(first_resp.tool_calls[0].clone())],
    });
    req.messages.push(Message::Tool {
        results: vec![ToolResult {
            call_id: "tu_1".into(),
            tool_name: "read_file".into(),
            content: "fn a() {}".into(),
            is_error: false,
        }],
    });
    let second_resp = chat_with(ProviderKind::Anthropic, &server, &req)
        .await
        .unwrap();
    assert_eq!(second_resp.text, "done");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let first_body: Value = requests[0].body_json().unwrap();
    assert_eq!(
        first_body["thinking"],
        json!({"type": "enabled", "budget_tokens": 4096})
    );
    assert_eq!(first_body["max_tokens"], 8192 + 4096);

    let second_body: Value = requests[1].body_json().unwrap();
    assert_eq!(
        second_body["messages"][1],
        json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "check caller", "signature": "sig_1"},
                {"type": "redacted_thinking", "data": "red_1"},
                {"type": "tool_use", "id": "tu_1", "name": "read_file", "input": {"path": "a.rs"}}
            ]
        })
    );
}

#[tokio::test]
async fn gemini_sends_thinking_budget_and_filters_thought_parts() {
    let server = serve(
        GEMINI_PATH,
        ok(json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "internal thought", "thought": true},
                        {"text": "visible answer"}
                    ]
                },
                "finishReason": "STOP"
            }]
        })),
    )
    .await;
    let req = ChatRequest::text("m-1", "sys", "hi", 200).with_effort(ReasoningEffort::Low);
    let resp = chat_with(ProviderKind::Gemini, &server, &req)
        .await
        .unwrap();
    assert_eq!(resp.text, "visible answer");
    assert_eq!(
        sent_body(&server).await["generationConfig"],
        json!({"maxOutputTokens": 200 + 1024, "thinkingConfig": {"thinkingBudget": 1024}})
    );
}

#[tokio::test]
async fn list_models_across_openai_anthropic_and_gemini() {
    let openai_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(ok(json!({
            "data": [{"id": "gpt-5"}, {"id": "gpt-5-mini"}, {"id": "gpt-5"}]
        })))
        .expect(1)
        .mount(&openai_server)
        .await;
    let openai_models = client_for(
        ProviderKind::Openai,
        Some(&openai_server.uri()),
        reqwest::Client::new(),
    )
    .list_models(KEY)
    .await
    .unwrap();
    assert_eq!(openai_models, vec!["gpt-5", "gpt-5-mini"]);

    let anthropic_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", KEY))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ok(json!({
            "data": [{"id": "claude-sonnet-4-5"}, {"id": "claude-haiku-4-5"}]
        })))
        .expect(1)
        .mount(&anthropic_server)
        .await;
    let anthropic_models = client_for(
        ProviderKind::Anthropic,
        Some(&anthropic_server.uri()),
        reqwest::Client::new(),
    )
    .list_models(KEY)
    .await
    .unwrap();
    assert_eq!(
        anthropic_models,
        vec!["claude-haiku-4-5", "claude-sonnet-4-5"]
    );

    let gemini_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .and(header("x-goog-api-key", KEY))
        .respond_with(ok(json!({
            "models": [
                {"name": "models/gemini-2.5-pro", "supportedGenerationMethods": ["generateContent"]},
                {"name": "models/text-embedding-004", "supportedGenerationMethods": ["embedContent"]}
            ]
        })))
        .expect(1)
        .mount(&gemini_server)
        .await;
    let gemini_models = client_for(
        ProviderKind::Gemini,
        Some(&gemini_server.uri()),
        reqwest::Client::new(),
    )
    .list_models(KEY)
    .await
    .unwrap();
    assert_eq!(gemini_models, vec!["gemini-2.5-pro"]);
}

#[tokio::test]
async fn openai_compatible_surfaces_http_200_error_body() {
    for (body, expected) in [
        (
            json!({"error": {"message": "Upstream failed", "code": 502}}),
            LlmError::Transient("provider status 502: Upstream failed".into()),
        ),
        (
            json!({"error": {"message": "Invalid auth", "code": 401}}),
            LlmError::Unauthorized,
        ),
        (
            json!({"error": {"message": "Rate limited", "code": 429}}),
            LlmError::RateLimited { retry_after: None },
        ),
        (
            json!({"error": {"message": "Insufficient credits", "code": 402}}),
            LlmError::Invalid("provider status 402: Insufficient credits".into()),
        ),
    ] {
        let server = serve(OPENAI_PATH, ok(body)).await;
        let err = chat(ProviderKind::OpenaiCompatible, &server)
            .await
            .unwrap_err();
        assert_eq!(err, expected);
    }
}

#[tokio::test]
async fn openai_compatible_reports_reasoning_budget_exhaustion() {
    let server = serve(
        OPENAI_PATH,
        ok(json!({
            "choices": [{
                "message": {"content": "", "reasoning": "thinking step 1..."},
                "finish_reason": "length"
            }]
        })),
    )
    .await;
    let err = chat(ProviderKind::OpenaiCompatible, &server)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        LlmError::Invalid("empty response: model exhausted max_tokens during reasoning".into())
    );
}
