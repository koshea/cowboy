//! ChatGPT plan usage end to end against mocks: the strict Responses body, the
//! OAuth token on every request (refreshed after a 401), the plan-usage failure
//! meanings, and the account's model catalogue.

use std::collections::BTreeMap;
use std::sync::Arc;

use cowboy_core::config::{ResolvedAuth, ResolvedModel, WireApi};
use cowboy_core::model::chatgpt::ChatGptFailures;
use cowboy_core::model::responses::ResponsesClient;
use cowboy_core::model::{list_chatgpt_models, Message, ModelClient, ToolDef};
use cowboy_core::oauth::chatgpt::{self, ChatGptAuth, ChatGptTokens, Endpoints, PLAN_SCOPE};
use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn model(base: &str) -> ResolvedModel {
    ResolvedModel {
        provider_name: "chatgpt".into(),
        base_url: format!("{base}/v1"),
        api: WireApi::Responses,
        auth: ResolvedAuth::Chatgpt,
        model: "gpt-6.1-sol".into(),
        temperature: 0.2,
        max_tokens: 4096,
        context_window: 200_000,
        reasoning_effort: None,
        top_p: Some(0.9),
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

fn signed_in(dir: &std::path::Path) {
    let now = cowboy_core::time::now_ms();
    chatgpt::save(
        dir,
        "chatgpt",
        &ChatGptAuth {
            client_id: "oaiapp_1".into(),
            access_token: Some("old-access".into()),
            refresh_token: Some("old-refresh".into()),
            expires_at_ms: now + 3_600_000,
            saved_at_ms: now,
            scopes: vec![PLAN_SCOPE.into()],
            ..Default::default()
        },
    )
    .unwrap();
}

fn client(dir: &std::path::Path, server: &MockServer) -> ResponsesClient {
    let tokens = ChatGptTokens::new(
        "chatgpt",
        dir.to_path_buf(),
        Endpoints {
            token_url: format!("{}/oauth/token", server.uri()),
        },
    )
    .unwrap();
    ResponsesClient::new(
        &model(&server.uri()),
        Arc::new(tokens),
        Arc::new(ChatGptFailures {
            provider: "chatgpt".into(),
        }),
        true,
    )
    .unwrap()
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn ok_stream() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        sse(&[
            json!({"type": "response.output_text.delta", "delta": "hi"}),
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 3, "output_tokens": 1}}}),
        ]),
        "text/event-stream",
    )
}

#[tokio::test]
async fn a_refused_token_is_refreshed_once_and_the_request_retried() {
    let dir = assert_fs::TempDir::new().unwrap();
    signed_in(dir.path());
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer old-access"))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"detail":"expired"}"#))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer new-access"))
        .respond_with(ok_stream())
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "new-access", "refresh_token": "new-refresh",
            "expires_in": 3600, "scope": PLAN_SCOPE
        })))
        .expect(1)
        .mount(&server)
        .await;
    let tools = [ToolDef {
        name: "shell".into(),
        description: "d".into(),
        parameters: json!({"type": "object"}),
    }];
    let r = client(dir.path(), &server)
        .chat(&[Message::system("sys"), Message::user("hi")], &tools, None)
        .await
        .unwrap();
    assert_eq!(r.content.as_deref(), Some("hi"));

    // The strict body: unstored, streamed, no sampling knobs, namespaced tools.
    let reqs = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&reqs.last().unwrap().body).unwrap();
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["stream"], json!(true));
    for k in ["temperature", "top_p", "max_output_tokens"] {
        assert!(body.get(k).is_none(), "{k} sent: {body}");
    }
    assert_eq!(body["tools"][0]["type"], "namespace");
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["instructions"], "sys");
}

#[tokio::test]
async fn a_usage_limit_is_reported_and_not_retried() {
    let dir = assert_fs::TempDir::new().unwrap();
    signed_in(dir.path());
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("x-request-id", "req_42")
                .set_body_json(json!({"error": {
                    "code": "subscription_sharing_usage_limit_exceeded", "message": "limit"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let e = client(dir.path(), &server)
        .chat(&[Message::user("x")], &[], None)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("Settings → Usage") && e.contains("req_42"),
        "{e}"
    );
}

#[tokio::test]
async fn a_usage_limit_mid_stream_is_reported() {
    let dir = assert_fs::TempDir::new().unwrap();
    signed_in(dir.path());
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                json!({"type": "response.output_text.delta", "delta": "par"}),
                json!({"type": "response.failed", "response": {"error": {
                    "code": "subscription_sharing_usage_limit_exceeded", "message": "limit"}}}),
            ]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let e = client(dir.path(), &server)
        .chat(&[Message::user("x")], &[], None)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("usage limit reached"), "{e}");
}

/// Recorded from the live ChatGPT route: a usage limit arrives as an `error`
/// event with the reason nested under `error`, after the stream opened.
#[tokio::test]
async fn a_usage_limit_error_event_as_chatgpt_sends_it() {
    let dir = assert_fs::TempDir::new().unwrap();
    signed_in(dir.path());
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[json!({"type": "error", "sequence_number": 3, "error": {
                "code": "subscription_sharing_usage_limit_exceeded",
                "message": "The ChatGPT user has reached their Subscription Sharing usage limit.",
                "param": null, "type": "invalid_request_error"}})]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let e = client(dir.path(), &server)
        .chat(&[Message::user("x")], &[], None)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("usage limit reached") && e.contains("Settings → Usage"),
        "{e}"
    );
}

#[tokio::test]
async fn an_admission_detail_is_shown_as_a_diagnostic() {
    let dir = assert_fs::TempDir::new().unwrap();
    signed_in(dir.path());
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string(r#"{"detail":"region not permitted"}"#),
        )
        .mount(&server)
        .await;
    let e = client(dir.path(), &server)
        .chat(&[Message::user("x")], &[], None)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("region not permitted") && e.contains("403"),
        "{e}"
    );
}

#[tokio::test]
async fn the_account_catalogue_keeps_listed_models_in_server_order() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [
            {"slug": "gpt-6.1-sol", "display_name": "GPT-6.1 Sol", "visibility": "list",
             "context_window": 500000, "default_reasoning_level": "low", "priority": 1,
             "description": "Latest workhorse model.",
             "supported_reasoning_levels": [{"effort": "low"}, {"effort": "xhigh"}]},
            {"slug": "internal-x", "display_name": "X", "visibility": "hide"},
            {"slug": "gpt-6-luna", "display_name": "GPT-6 Luna", "visibility": "list"},
        ]})))
        .mount(&server)
        .await;
    let m = list_chatgpt_models(&format!("{}/v1", server.uri()), "tok", &BTreeMap::new())
        .await
        .unwrap();
    let got: Vec<(&str, Option<&str>)> = m
        .iter()
        .map(|e| (e.id.as_str(), e.label.as_deref()))
        .collect();
    assert_eq!(
        got,
        [
            ("gpt-6.1-sol", Some("GPT-6.1 Sol")),
            ("gpt-6-luna", Some("GPT-6 Luna"))
        ]
    );
}

#[tokio::test]
async fn not_signed_in_says_how_to_sign_in() {
    let dir = assert_fs::TempDir::new().unwrap();
    let server = MockServer::start().await;
    let e = client(dir.path(), &server)
        .chat(&[Message::user("x")], &[], None)
        .await
        .unwrap_err();
    assert!(matches!(e, cowboy_core::Error::SignInRequired(_)), "{e}");
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "no request without a token"
    );
}
