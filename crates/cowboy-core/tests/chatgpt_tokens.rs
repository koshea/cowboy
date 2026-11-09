//! The ChatGPT credential record's refresh against a mock token endpoint:
//! when it refreshes, what it sends, how it fails, and that concurrent callers
//! (stand-ins for separate subagent processes) share a single refresh.

use std::path::Path;
use std::sync::Arc;

use cowboy_core::model::TokenSource;
use cowboy_core::oauth::chatgpt::{self, ChatGptAuth, ChatGptTokens, Endpoints, PLAN_SCOPE};
use cowboy_core::time::now_ms;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn record(access_expires_in_ms: i64) -> ChatGptAuth {
    let now = now_ms() as i64;
    ChatGptAuth {
        issuer: "https://auth.openai.com".into(),
        client_id: "oaiapp_test".into(),
        ext_agent_host_id: "urn:uuid:00000000-0000-4000-8000-000000000000".into(),
        subject: Some("user-1".into()),
        email: Some("u@example.com".into()),
        id_token: Some("id.tok.en".into()),
        access_token: Some("old-access".into()),
        refresh_token: Some("old-refresh".into()),
        token_type: Some("Bearer".into()),
        expires_in: 3600,
        expires_at_ms: (now + access_expires_in_ms) as u64,
        saved_at_ms: now as u64,
        earliest_refresh_at_ms: None,
        scopes: vec!["openid".into(), "offline_access".into(), PLAN_SCOPE.into()],
    }
}

fn source(dir: &Path, server: &MockServer) -> ChatGptTokens {
    ChatGptTokens::new(
        "chatgpt",
        dir.to_path_buf(),
        Endpoints {
            token_url: format!("{}/api/accounts/oauth/token", server.uri()),
        },
    )
    .unwrap()
}

fn refreshed() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "access_token": "new-access",
        "refresh_token": "new-refresh",
        "token_type": "Bearer",
        "expires_in": 3600,
        "scope": format!("openid offline_access {PLAN_SCOPE}"),
    }))
}

#[tokio::test]
async fn a_fresh_token_is_used_without_refreshing() {
    let dir = assert_fs::TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(refreshed())
        .expect(0)
        .mount(&server)
        .await;
    chatgpt::save(dir.path(), "chatgpt", &record(60 * 60 * 1000)).unwrap();
    assert_eq!(
        source(dir.path(), &server).token().await.unwrap(),
        "old-access"
    );
}

#[tokio::test]
async fn an_expired_token_is_refreshed_with_the_issued_client_and_no_scope() {
    let dir = assert_fs::TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("client_id=oaiapp_test"))
        .and(body_string_contains("refresh_token=old-refresh"))
        .and(body_string_contains(
            "resource=https%3A%2F%2Fapi.openai.com%2Fv1",
        ))
        .respond_with(refreshed())
        .expect(1)
        .mount(&server)
        .await;
    chatgpt::save(dir.path(), "chatgpt", &record(-1000)).unwrap();

    assert_eq!(
        source(dir.path(), &server).token().await.unwrap(),
        "new-access"
    );
    let req = &server.received_requests().await.unwrap()[0];
    let body = String::from_utf8_lossy(&req.body);
    assert!(!body.contains("scope="), "refresh must omit scope: {body}");

    // The rotated refresh token replaced the old one, together with the access token.
    let rec = chatgpt::load(dir.path(), "chatgpt").unwrap().unwrap();
    assert_eq!(rec.access_token.as_deref(), Some("new-access"));
    assert_eq!(rec.refresh_token.as_deref(), Some("new-refresh"));
    assert!(rec.expires_at_ms > now_ms() + 50 * 60 * 1000);
    assert_eq!(rec.client_id, "oaiapp_test", "the registration is kept");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(chatgpt::record_path(dir.path(), "chatgpt"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

/// Eight callers with their own token sources and caches — what eight subagent
/// processes look like — all find the token expired at once. Exactly one refresh
/// reaches the endpoint; the rest find its result under the lock. Without the
/// lock-and-re-read, seven of them would spend a refresh token that the first one
/// had just rotated away (`refresh_token_reused`) and sign the user out.
#[test]
fn concurrent_callers_share_one_refresh() {
    let dir = assert_fs::TempDir::new().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(refreshed().set_delay(std::time::Duration::from_millis(200)))
            .mount(&server)
            .await;
        server
    });
    chatgpt::save(dir.path(), "chatgpt", &record(-1000)).unwrap();
    let token_url = format!("{}/api/accounts/oauth/token", server.uri());
    let dir_path = Arc::new(dir.path().to_path_buf());

    let threads: Vec<_> = (0..8)
        .map(|_| {
            let token_url = token_url.clone();
            let dir_path = dir_path.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    let s = ChatGptTokens::new(
                        "chatgpt",
                        dir_path.as_ref().clone(),
                        Endpoints { token_url },
                    )
                    .unwrap();
                    s.token().await.unwrap()
                })
            })
        })
        .collect();
    for t in threads {
        assert_eq!(t.join().unwrap(), "new-access");
    }
    let n = rt.block_on(server.received_requests()).unwrap().len();
    assert_eq!(n, 1, "exactly one refresh for eight concurrent callers");
}

#[tokio::test]
async fn a_terminal_refresh_error_clears_tokens_keeps_the_registration_and_says_sign_in() {
    let dir = assert_fs::TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(serde_json::json!({"error": "refresh_token_reused"})),
        )
        .mount(&server)
        .await;
    chatgpt::save(dir.path(), "chatgpt", &record(-1000)).unwrap();
    let err = source(dir.path(), &server).token().await.unwrap_err();
    assert!(
        matches!(err, cowboy_core::Error::SignInRequired(_)),
        "{err}"
    );
    assert!(
        err.to_string().contains("cowboy providers login chatgpt"),
        "{err}"
    );

    let rec = chatgpt::load(dir.path(), "chatgpt").unwrap().unwrap();
    assert!(rec.access_token.is_none() && rec.refresh_token.is_none());
    assert_eq!(rec.client_id, "oaiapp_test");
    assert_eq!(
        rec.id_token.as_deref(),
        Some("id.tok.en"),
        "kept for id_token_hint"
    );
    assert_eq!(rec.email.as_deref(), Some("u@example.com"));
}

#[tokio::test]
async fn a_transient_refresh_error_keeps_the_credentials() {
    let dir = assert_fs::TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    chatgpt::save(dir.path(), "chatgpt", &record(-1000)).unwrap();
    let err = source(dir.path(), &server).token().await.unwrap_err();
    assert!(
        !matches!(err, cowboy_core::Error::SignInRequired(_)),
        "{err}"
    );
    assert!(err.to_string().contains("kept"), "{err}");
    let rec = chatgpt::load(dir.path(), "chatgpt").unwrap().unwrap();
    assert_eq!(rec.refresh_token.as_deref(), Some("old-refresh"));
    // Retried with backoff before giving up.
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn a_token_refreshed_by_another_process_is_picked_up_on_401() {
    let dir = assert_fs::TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(refreshed())
        .expect(0)
        .mount(&server)
        .await;
    chatgpt::save(dir.path(), "chatgpt", &record(60 * 60 * 1000)).unwrap();
    let s = source(dir.path(), &server);
    assert_eq!(s.token().await.unwrap(), "old-access");

    // Another process refreshes; this one's cached token is now refused.
    let mut rec = record(60 * 60 * 1000);
    rec.access_token = Some("other-process-access".into());
    chatgpt::save(dir.path(), "chatgpt", &rec).unwrap();
    assert!(s.on_unauthorized("old-access").await.unwrap());
    assert_eq!(s.token().await.unwrap(), "other-process-access");
}

#[tokio::test]
async fn a_refused_current_token_forces_a_refresh() {
    let dir = assert_fs::TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(refreshed())
        .expect(1)
        .mount(&server)
        .await;
    chatgpt::save(dir.path(), "chatgpt", &record(60 * 60 * 1000)).unwrap();
    let s = source(dir.path(), &server);
    assert!(s.on_unauthorized("old-access").await.unwrap());
    assert_eq!(s.token().await.unwrap(), "new-access");
}

#[tokio::test]
async fn no_record_or_no_plan_scope_says_how_to_fix_it() {
    let dir = assert_fs::TempDir::new().unwrap();
    let server = MockServer::start().await;
    let err = source(dir.path(), &server).token().await.unwrap_err();
    assert!(
        matches!(err, cowboy_core::Error::SignInRequired(_)),
        "{err}"
    );

    let mut rec = record(60 * 60 * 1000);
    rec.scopes.retain(|s| s != PLAN_SCOPE);
    chatgpt::save(dir.path(), "chatgpt", &rec).unwrap();
    let err = source(dir.path(), &server).token().await.unwrap_err();
    assert!(err.to_string().contains("ChatGPT plan"), "{err}");
}

#[test]
fn the_record_never_prints_its_tokens() {
    let dbg = format!("{:?}", record(0));
    assert!(
        !dbg.contains("old-access") && !dbg.contains("old-refresh"),
        "{dbg}"
    );
}
