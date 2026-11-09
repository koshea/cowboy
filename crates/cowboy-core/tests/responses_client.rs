//! The Responses API client against a mock endpoint: event-stream assembly
//! (text, reasoning, parallel tool calls, usage), the terminal events, and the
//! shared transport behaviour (retry, stall).

use std::collections::BTreeMap;

use cowboy_core::config::{ResolvedAuth, ResolvedModel, WireApi};
use cowboy_core::model::{build_client, Delta, Message, ModelClient, ToolDef};
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn profile(base_url: String) -> ResolvedModel {
    ResolvedModel {
        provider_name: "bifrost".into(),
        base_url,
        api: WireApi::Responses,
        auth: ResolvedAuth::ApiKey("test-key".into()),
        model: "gpt-x".into(),
        temperature: 0.2,
        max_tokens: 256,
        context_window: 8192,
        reasoning_effort: None,
        top_p: None,
        stop: Vec::new(),
        extra: BTreeMap::new(),
        headers: BTreeMap::new(),
        input_cost_per_mtok: None,
        output_cost_per_mtok: None,
        cached_input_cost_per_mtok: None,
        anthropic_cache: false,
        stream_idle_timeout_seconds: None,
    }
}

/// An SSE body in the Responses shape (`event:` + `data:` per event).
fn sse(events: &[Value]) -> String {
    let mut s = String::new();
    for e in events {
        s.push_str(&format!(
            "event: {}\ndata: {e}\n\n",
            e["type"].as_str().unwrap()
        ));
    }
    s
}

fn completed(input: u64, output: u64, cached: u64) -> Value {
    json!({"type": "response.completed", "response": {
        "status": "completed",
        "usage": {"input_tokens": input, "output_tokens": output,
                  "input_tokens_details": {"cached_tokens": cached}}
    }})
}

async fn serve(events: &[Value]) -> (MockServer, Box<dyn ModelClient>) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse(events), "text/event-stream"))
        .mount(&server)
        .await;
    let client = build_client(&profile(format!("{}/v1", server.uri()))).unwrap();
    (server, client)
}

#[tokio::test]
async fn streams_text_and_reasoning_and_reports_usage() {
    let (_s, client) = serve(&[
        json!({"type": "response.created", "response": {}}),
        json!({"type": "response.reasoning_summary_text.delta", "delta": "thinking…"}),
        json!({"type": "response.output_text.delta", "delta": "Hel"}),
        json!({"type": "response.output_text.delta", "delta": "lo"}),
        completed(120, 7, 100),
    ])
    .await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let r = client
        .chat(&[Message::system("s"), Message::user("hi")], &[], Some(tx))
        .await
        .unwrap();
    assert_eq!(r.content.as_deref(), Some("Hello"));
    assert_eq!(r.reasoning.as_deref(), Some("thinking…"));
    assert!(!r.truncated);
    let u = r.usage.unwrap();
    assert_eq!(
        (u.prompt_tokens, u.completion_tokens, u.cached_prompt_tokens),
        (120, 7, 100)
    );
    let mut got = Vec::new();
    while let Ok(d) = rx.try_recv() {
        got.push(d);
    }
    assert_eq!(
        got,
        [
            Delta::Reasoning("thinking…".into()),
            Delta::Content("Hel".into()),
            Delta::Content("lo".into())
        ]
    );
}

#[tokio::test]
async fn assembles_parallel_tool_calls_by_output_index() {
    let (_s, client) = serve(&[
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"type": "function_call", "call_id": "call_a", "name": "shell", "arguments": ""}}),
        json!({"type": "response.output_item.added", "output_index": 1,
               "item": {"type": "function_call", "call_id": "call_b", "name": "read", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"pa"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"cmd\":"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "th\":\"a\"}"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "\"ls\"}"}),
        json!({"type": "response.output_item.done", "output_index": 0,
               "item": {"type": "function_call", "call_id": "call_a", "name": "shell", "arguments": "{\"cmd\":\"ls\"}"}}),
        completed(10, 5, 0),
    ])
    .await;
    let tools = [ToolDef {
        name: "shell".into(),
        description: "d".into(),
        parameters: json!({"type": "object"}),
    }];
    let r = client
        .chat(&[Message::user("go")], &tools, None)
        .await
        .unwrap();
    assert_eq!(r.tool_calls.len(), 2);
    assert_eq!(
        (
            r.tool_calls[0].id.as_str(),
            r.tool_calls[0].name.as_str(),
            r.tool_calls[0].arguments.as_str()
        ),
        ("call_a", "shell", r#"{"cmd":"ls"}"#)
    );
    assert_eq!(
        (
            r.tool_calls[1].id.as_str(),
            r.tool_calls[1].arguments.as_str()
        ),
        ("call_b", r#"{"path":"a"}"#)
    );
}

#[tokio::test]
async fn sends_the_full_history_unstored_with_instructions() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(json!({
            "model": "gpt-x",
            "instructions": "sys",
            "store": false,
            "stream": true,
            "input": [{"type": "message", "role": "user", "content": "hi"}],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                json!({"type": "response.output_text.delta", "delta": "ok"}),
                completed(1, 1, 0),
            ]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let client = build_client(&profile(format!("{}/v1", server.uri()))).unwrap();
    let r = client
        .chat(&[Message::system("sys"), Message::user("hi")], &[], None)
        .await
        .unwrap();
    assert_eq!(r.content.as_deref(), Some("ok"));
}

#[tokio::test]
async fn an_incomplete_response_is_a_truncation() {
    let (_s, client) = serve(&[
        json!({"type": "response.output_text.delta", "delta": "partial"}),
        json!({"type": "response.incomplete", "response": {
            "incomplete_details": {"reason": "max_output_tokens"},
            "usage": {"input_tokens": 5, "output_tokens": 256}}}),
    ])
    .await;
    let r = client.chat(&[Message::user("x")], &[], None).await.unwrap();
    assert!(r.truncated);
    assert_eq!(r.content.as_deref(), Some("partial"));
}

#[tokio::test]
async fn a_failed_response_keeps_its_code() {
    let (_s, client) = serve(&[
        json!({"type": "response.output_text.delta", "delta": "a"}),
        json!({"type": "response.failed", "response": {"error": {
            "code": "server_error", "message": "boom"}}}),
    ])
    .await;
    let err = client
        .chat(&[Message::user("x")], &[], None)
        .await
        .unwrap_err();
    let s = err.to_string();
    assert!(s.contains("server_error") && s.contains("boom"), "{s}");
}

#[tokio::test]
async fn an_error_event_names_its_param() {
    let (_s, client) = serve(&[json!({"type": "error", "code": "invalid_request",
        "message": "bad", "param": "tools[0]"})])
    .await;
    let s = client
        .chat(&[Message::user("x")], &[], None)
        .await
        .unwrap_err()
        .to_string();
    assert!(s.contains("tools[0]"), "{s}");
}

#[tokio::test]
async fn a_stream_that_just_ends_is_an_error_not_an_empty_answer() {
    let (_s, client) =
        serve(&[json!({"type": "response.output_text.delta", "delta": "half"})]).await;
    let s = client
        .chat(&[Message::user("x")], &[], None)
        .await
        .unwrap_err()
        .to_string();
    assert!(s.contains("ended before the response completed"), "{s}");
}

#[tokio::test]
async fn a_missing_model_is_reported_as_unavailable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(404).set_body_string(
            r#"{"error":{"code":"model_not_found","message":"The model `gpt-x` does not exist"}}"#,
        ))
        .mount(&server)
        .await;
    let client = build_client(&profile(format!("{}/v1", server.uri()))).unwrap();
    let err = client
        .chat(&[Message::user("x")], &[], None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, cowboy_core::Error::ModelUnavailable(_)),
        "{err}"
    );
}

#[tokio::test]
async fn retries_after_a_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                json!({"type": "response.output_text.delta", "delta": "ok"}),
                completed(1, 1, 0),
            ]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let client = build_client(&profile(format!("{}/v1", server.uri()))).unwrap();
    let r = client.chat(&[Message::user("x")], &[], None).await.unwrap();
    assert_eq!(r.content.as_deref(), Some("ok"));
}

#[tokio::test]
async fn a_keyless_provider_sends_no_authorization() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer "))
        .respond_with(ResponseTemplate::new(400))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse(&[completed(1, 0, 0)]), "text/event-stream"),
        )
        .mount(&server)
        .await;
    let mut p = profile(format!("{}/v1", server.uri()));
    p.auth = ResolvedAuth::ApiKey(String::new());
    let client = build_client(&p).unwrap();
    client.chat(&[Message::user("x")], &[], None).await.unwrap();
    let reqs = server.received_requests().await.unwrap();
    assert!(reqs[0].headers.get("authorization").is_none());
}

#[tokio::test]
async fn mid_stream_stall_errors_after_the_idle_timeout() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 65536];
        let _ = sock.read(&mut buf).await;
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                    transfer-encoding: chunked\r\n\r\n";
        sock.write_all(head.as_bytes()).await.unwrap();
        let event = sse(&[json!({"type": "response.output_text.delta", "delta": "started"})]);
        let chunk = format!("{:x}\r\n{event}\r\n", event.len());
        sock.write_all(chunk.as_bytes()).await.unwrap();
        sock.flush().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    });
    let mut p = profile(format!("http://{addr}/v1"));
    p.stream_idle_timeout_seconds = Some(1);
    let client = build_client(&p).unwrap();
    let err = client
        .chat(&[Message::user("hi")], &[], None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("stalled"), "{err}");
}

#[test]
fn minimal_reasoning_variant_exists_until_already_minimal() {
    let mut p = profile("http://x/v1".into());
    let c = build_client(&p).unwrap();
    assert!(c.with_minimal_reasoning().is_some());
    p.reasoning_effort = Some(cowboy_core::config::ReasoningEffort::Minimal);
    assert!(build_client(&p).unwrap().with_minimal_reasoning().is_none());
}
