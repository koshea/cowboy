//! Tests the OpenAI-compatible streaming client against a mock endpoint.
//!
//! `wiremock` serves a complete SSE body (it cannot stream incrementally, but
//! the client's parser handles a full event-stream body), letting us verify
//! that streamed content deltas and tool-call chunks are assembled correctly.

use std::collections::BTreeMap;

use cowboy_core::config::ModelProfile;
use cowboy_core::model::{Message, ModelClient, OpenAiClient, ToolDef};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn profile(base_url: String) -> ModelProfile {
    ModelProfile {
        base_url,
        api_key_env: "COWBOY_TEST_KEY_UNSET".into(),
        model: "test-model".into(),
        temperature: 0.0,
        max_tokens: 256,
        context_window: 8192,
        headers: BTreeMap::new(),
    }
}

/// Build an SSE event-stream body from chunk JSON values.
fn sse(chunks: &[serde_json::Value]) -> String {
    let mut s = String::new();
    for c in chunks {
        s.push_str("data: ");
        s.push_str(&c.to_string());
        s.push_str("\n\n");
    }
    s.push_str("data: [DONE]\n\n");
    s
}

fn content_chunk(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "c", "object": "chat.completion.chunk", "created": 0, "model": "test-model",
        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
    })
}

#[tokio::test]
async fn streams_and_assembles_text() {
    let server = MockServer::start().await;
    let body = sse(&[content_chunk("Hello, "), content_chunk("world")]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let client = OpenAiClient::from_profile(&profile(format!("{}/v1", server.uri()))).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let resp = client
        .chat(&[Message::user("hi")], &[], Some(tx))
        .await
        .unwrap();

    assert_eq!(resp.content.as_deref(), Some("Hello, world"));
    assert!(resp.tool_calls.is_empty());

    // Deltas were streamed in order.
    let mut got = String::new();
    while let Ok(piece) = rx.try_recv() {
        got.push_str(&piece);
    }
    assert_eq!(got, "Hello, world");
}

#[tokio::test]
async fn forwards_custom_headers() {
    let server = MockServer::start().await;
    // The mock only matches if the custom header is present.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("x-cowboy-test", "abc123"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse(&[content_chunk("ok")]), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let mut p = profile(format!("{}/v1", server.uri()));
    p.headers.insert("x-cowboy-test".into(), "abc123".into());
    let client = OpenAiClient::from_profile(&p).unwrap();
    let resp = client
        .chat(&[Message::user("hi")], &[], None)
        .await
        .unwrap();
    assert_eq!(resp.content.as_deref(), Some("ok"));
}

#[tokio::test]
async fn chat_is_cancelled_by_dropping_the_future() {
    let server = MockServer::start().await;
    // Respond, but only after a delay; we cancel before it arrives.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse(&[content_chunk("late")]), "text/event-stream")
                .set_delay(std::time::Duration::from_secs(5)),
        )
        .mount(&server)
        .await;

    let client = OpenAiClient::from_profile(&profile(format!("{}/v1", server.uri()))).unwrap();
    let msgs = [Message::user("hi")];
    let fut = client.chat(&msgs, &[], None);
    // Dropping the future on timeout cancels the in-flight request.
    let res = tokio::time::timeout(std::time::Duration::from_millis(300), fut).await;
    assert!(res.is_err(), "expected cancellation (timeout), got {res:?}");
}

#[tokio::test]
async fn assembles_streamed_tool_call() {
    let server = MockServer::start().await;
    // Tool call split across two chunks (id+name first, then argument fragments).
    let chunk1 = serde_json::json!({
        "id": "c", "object": "chat.completion.chunk", "created": 0, "model": "test-model",
        "choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call_1", "type": "function",
             "function": {"name": "shell", "arguments": "{\"command\":\""}}
        ]}, "finish_reason": null}]
    });
    let chunk2 = serde_json::json!({
        "id": "c", "object": "chat.completion.chunk", "created": 0, "model": "test-model",
        "choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "function": {"arguments": "ls -la\"}"}}
        ]}, "finish_reason": "tool_calls"}]
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse(&[chunk1, chunk2]), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let tools = vec![ToolDef {
        name: "shell".into(),
        description: "run a shell command".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"]
        }),
    }];
    let client = OpenAiClient::from_profile(&profile(format!("{}/v1", server.uri()))).unwrap();
    let resp = client
        .chat(&[Message::user("list files")], &tools, None)
        .await
        .unwrap();

    assert_eq!(resp.tool_calls.len(), 1);
    assert_eq!(resp.tool_calls[0].name, "shell");
    assert_eq!(resp.tool_calls[0].id, "call_1");
    assert_eq!(resp.tool_calls[0].arguments, "{\"command\":\"ls -la\"}");
}
