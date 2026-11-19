use std::time::Duration;

use adus_llm::{ChatRequest, ChatResponse, LlmError, ProviderKind, client_for, default_base_url};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "sk-secret-key";

fn request() -> ChatRequest {
    ChatRequest {
        model: "m-1".into(),
        system: "be brief".into(),
        user: "hello".into(),
        max_tokens: 321,
    }
}

async fn chat(kind: ProviderKind, server: &MockServer) -> Result<ChatResponse, LlmError> {
    client_for(kind, Some(&server.uri()), reqwest::Client::new())
        .chat(KEY, &request())
        .await
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
            "choices": [{"message": {"role": "assistant", "content": "hi there"}}],
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
            input_tokens: 11,
            output_tokens: 7
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
            "system": "be brief",
            "max_tokens": 321,
            "messages": [{"role": "user", "content": "hello"}],
        })))
        .respond_with(ok(json!({
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "part one, "},
                {"type": "text", "text": "part two"},
            ],
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
            input_tokens: 5,
            output_tokens: 9
        }
    );
}

#[tokio::test]
async fn anthropic_no_text_blocks_is_invalid() {
    let server = serve(
        ANTHROPIC_PATH,
        ok(json!({"content": [{"type": "tool_use", "id": "t"}]})),
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
            "candidates": [{"content": {"parts": [{"text": "a"}, {"text": "b"}]}}],
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
            input_tokens: 3,
            output_tokens: 4
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
