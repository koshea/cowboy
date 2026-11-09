//! The Sign in with ChatGPT steps that do not need a browser: the authorize URL,
//! the callback, the code exchange, ID-token verification against a JWKS, and
//! revocation — with a throwaway RSA key standing in for OpenAI's.

use cowboy_core::oauth::chatgpt::{ChatGptAuth, TokenResponse, PLAN_SCOPE};
use cowboy_core::oauth::login::{self, Attempt, Callback, Issuer, DYNAMIC_CLIENT};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A 2048-bit key generated for these tests only. Signs nothing real.
const TEST_KEY: &str = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEAolvIFKoo+FMZdxrY8C4WRVFNl3uJHPcWUGAjEdo9QFw2i5U5\nOkHj690wTkzzC2BK3rNJzHDywZ4YZ9ovn4XqiQ/mtObNToOTx9ToOA7iAz+gkBz4\n2kkf+VWZXFn6m4OeSB0jQXJHvtq+a5TYET1yHGuk+8zvsooy6L99zr3wBIUqjX6u\namvyN/M6KDLvVNtShj5G2BRB5VGluHImlGOAiMgtglC7jz4RwvrTDksAYitfdHSX\nnZXGwuYgZ/DGwdTFGYdn+h1wHeg2LLZXpkt+R5AxRQqCkJUTCPwhAMTYx5RbsxpM\nDD2/n3NU0E1LpgU7klSPQkuBUgyBBpx1leIIrQIDAQABAoIBAAZ7E2l/wVLoPkP6\n1l3/+PObYzWAwOBfHIzoyX2U8Iz5IVRBCJ/CsY5greTyPWFe3p41GeUHPSltXhwH\nIY+EkPAXqmXeMKPJrMR1Qy+5qFvo1kfqYCSWdqhFOnHtRYufnVnTj+y02xhLMv6o\nWleV1FA+G7/RVmASFCYmPqdZ/ZKwjc8BWW2YAbc7mW+kkTvoiX0XA+Z5dkiRW8FS\n8RYampLoSm8YIeVwE56wX6Rm0ARooxrTTkdlYEWDsZCmntXYaYEBQqH6WWSkMfJG\nycMnDkxEfxnjfW0G/2lBARvBnqMdXFYNq+s6RaF5/qnpdpEo5hrCtqSSjsS3EVd6\nWovNQSECgYEA1bnqESDKzFwh00Jxje0wtj/iPX8HJq48XfbdkoCV0zOAd+SHywEm\nW8Q3jPJ+julZVk4wQEqAuG+Qf01+iLA6XtbVn05IUFlpsrEyExmQsFTE9g52UwtV\nSPeHgbZUpbaUDyfwJGRCcwY54dRD50+qXEDjTYUnkcONfTj107ychNsCgYEAwnjZ\ndwe6RHcXAye2VZ2C8EZPaeMYYkIUEwqD5vWxbWGGmX44AsyHjvjkUWJolfOq8Iqh\nZSbiM57iowbbMqvnL1IU2lxGT58m+EfGarh9mSurli9Hh+9JiEg3U6s/bVGWqieL\nCdGmqi05b3iAV439yEG0cljQn8ZKwJ5RGc8dGxcCgYEAgH4inCOT6SMm7uUOch58\n38UulnxNZ1LvViq58X8/FFftT6dkgIbzcc5Y+3M65V6jaEDfv3dLRhdEz1Re03zn\nyubsTbd5wFzRMC5aBaOkGm8Mb0YomyrAHns6y6A76dkH0MjZFnf0f0Tp/bxpCqMZ\nsBcO0SdRr8/au8NIAAFwNGsCgYEAspQw9enltTs6qY4NFH+InsWh5RVB7fDJ6FZO\nm4bggBX/ePH70y4oCTUqzEU2zwQr46kZ3f4f19E+QwBnxxcuOzj/pyj28+Ss729R\nl+yEgWQls8sEv6IPfxL5o15s5yD13phjSAf6KOlQhbk8+amM8xjK7/n40c9QE6MN\nzEpxas8CgYAh8eftLIV6M9rsTGlfE6OEg3i4pmDLcf5dMIyUe7PqTHM1KS2GQZv8\ntQl/Ha1ueGpy5aLv8jSnzAyCGQkz9YoyfJiEfJyuwNAdoaXSM7GoxpBNbV0W2VMF\nXQKvq9H5TgW1n/rkt8D8RtWOknXtKGyNHfaAET49LwKT5Do45YvHmQ==\n-----END RSA PRIVATE KEY-----";
const TEST_N: &str = "olvIFKoo-FMZdxrY8C4WRVFNl3uJHPcWUGAjEdo9QFw2i5U5OkHj690wTkzzC2BK3rNJzHDywZ4YZ9ovn4XqiQ_mtObNToOTx9ToOA7iAz-gkBz42kkf-VWZXFn6m4OeSB0jQXJHvtq-a5TYET1yHGuk-8zvsooy6L99zr3wBIUqjX6uamvyN_M6KDLvVNtShj5G2BRB5VGluHImlGOAiMgtglC7jz4RwvrTDksAYitfdHSXnZXGwuYgZ_DGwdTFGYdn-h1wHeg2LLZXpkt-R5AxRQqCkJUTCPwhAMTYx5RbsxpMDD2_n3NU0E1LpgU7klSPQkuBUgyBBpx1leIIrQ";

fn jwks() -> jsonwebtoken::jwk::JwkSet {
    serde_json::from_value(json!({"keys": [{
        "kty": "RSA", "kid": "k1", "use": "sig", "alg": "RS256", "n": TEST_N, "e": "AQAB"
    }]}))
    .unwrap()
}

fn id_token(aud: &str, nonce: &str, exp_offset: i64, sub: &str) -> String {
    let now = (cowboy_core::time::now_ms() / 1000) as i64;
    let mut h = Header::new(Algorithm::RS256);
    h.kid = Some("k1".into());
    encode(
        &h,
        &json!({"iss": "https://auth.openai.com", "aud": aud, "sub": sub,
                "email": "u@example.com", "nonce": nonce,
                "iat": now, "exp": now + exp_offset}),
        &EncodingKey::from_rsa_pem(TEST_KEY.as_bytes()).unwrap(),
    )
    .unwrap()
}

fn issuer(base: &str) -> Issuer {
    Issuer {
        issuer: "https://auth.openai.com".into(),
        authorize_url: format!("{base}/api/accounts/authorize"),
        token_url: format!("{base}/api/accounts/oauth/token"),
        jwks_url: format!("{base}/jwks"),
        revocation_url: Some(format!("{base}/revoke")),
    }
}

fn fixed_attempt(client_id: Option<&str>) -> Attempt {
    Attempt {
        state: "STATE".into(),
        nonce: "NONCE".into(),
        verifier: "VERIFIER-0123456789-0123456789-0123456789".into(),
        redirect_uri: "http://127.0.0.1:1455/auth/callback".into(),
        client_id: client_id.map(str::to_string),
    }
}

fn prior() -> ChatGptAuth {
    ChatGptAuth {
        client_id: "oaiapp_123".into(),
        subject: Some("user-1".into()),
        email: Some("u@example.com".into()),
        id_token: Some("old.id.token".into()),
        ..Default::default()
    }
}

fn query(url: &str) -> std::collections::BTreeMap<String, String> {
    reqwest::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

#[test]
fn a_first_sign_in_registers_with_the_dynamic_client() {
    let url = fixed_attempt(None).authorize_url(
        &issuer("https://auth.openai.com"),
        "urn:uuid:host",
        None,
        false,
    );
    let q = query(&url);
    assert!(url.starts_with("https://auth.openai.com/api/accounts/authorize?"));
    assert_eq!(q["client_id"], DYNAMIC_CLIENT);
    assert_eq!(q["agent_name_hint"], "Cowboy");
    assert_eq!(q["ext_agent_host_id"], "urn:uuid:host");
    assert_eq!(q["redirect_uri"], "http://127.0.0.1:1455/auth/callback");
    assert_eq!(q["resource"], "https://api.openai.com/v1");
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["scope"].contains(PLAN_SCOPE) && q["scope"].contains("offline_access"));
    assert!(!q.contains_key("id_token_hint") && !q.contains_key("prompt"));
    insta::assert_snapshot!(url);
}

#[test]
fn a_returning_sign_in_uses_the_issued_client_and_hints() {
    let p = prior();
    let a = Attempt::new("http://127.0.0.1:1457/auth/callback".into(), Some(&p));
    assert_eq!(a.client_id.as_deref(), Some("oaiapp_123"));
    let q = query(&a.authorize_url(
        &issuer("https://auth.openai.com"),
        "urn:uuid:host",
        Some(&p),
        true,
    ));
    assert_eq!(q["client_id"], "oaiapp_123");
    assert_eq!(q["id_token_hint"], "old.id.token");
    assert_eq!(q["login_hint"], "u@example.com");
    assert_eq!(q["prompt"], "consent");
    assert!(
        !q.contains_key("agent_name_hint"),
        "only on first registration"
    );
    // Fresh secrets every attempt.
    let b = Attempt::new(a.redirect_uri.clone(), Some(&p));
    assert_ne!(a.state, b.state);
    assert_ne!(a.verifier, b.verifier);
}

#[test]
fn the_pkce_challenge_is_unpadded_base64url_sha256() {
    let mut a = fixed_attempt(None);
    // RFC 7636 appendix B.
    a.verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into();
    assert_eq!(a.challenge(), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
}

#[test]
fn the_callback_is_checked_before_it_is_believed() {
    assert!(login::parse_callback("code=c&state=OTHER", "STATE").is_err());
    // A denial is only believed with the right state, too.
    assert!(login::parse_callback("error=access_denied&state=OTHER", "STATE").is_err());
    assert_eq!(
        login::parse_callback("error=access_denied&state=STATE", "STATE").unwrap(),
        Callback::Denied
    );
    assert_eq!(
        login::parse_callback(
            "code=abc&state=STATE&client_id=oaiapp_9&scope=openid",
            "STATE"
        )
        .unwrap(),
        Callback::Code {
            code: "abc".into(),
            client_id: Some("oaiapp_9".into())
        }
    );
    assert!(
        login::parse_callback("state=STATE", "STATE").is_err(),
        "no code"
    );
}

#[test]
fn the_issued_client_id_is_required_and_never_swapped() {
    let first = fixed_attempt(None);
    assert_eq!(
        login::issued_client_id(&first, Some("oaiapp_9")).unwrap(),
        "oaiapp_9"
    );
    assert!(
        login::issued_client_id(&first, None).is_err(),
        "registration incomplete"
    );
    assert!(login::issued_client_id(&first, Some(DYNAMIC_CLIENT)).is_err());
    let back = fixed_attempt(Some("oaiapp_123"));
    assert_eq!(login::issued_client_id(&back, None).unwrap(), "oaiapp_123");
    assert_eq!(
        login::issued_client_id(&back, Some("oaiapp_123")).unwrap(),
        "oaiapp_123"
    );
    assert!(login::issued_client_id(&back, Some("oaiapp_other")).is_err());
}

#[tokio::test]
async fn the_code_is_exchanged_with_pkce_and_no_secret() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/oauth/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("client_id=oaiapp_9"))
        .and(body_string_contains("code=abc"))
        .and(body_string_contains("code_verifier=VERIFIER"))
        .and(body_string_contains(
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback",
        ))
        .and(body_string_contains(
            "resource=https%3A%2F%2Fapi.openai.com%2Fv1",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "a", "refresh_token": "r", "id_token": "i",
            "expires_in": 3600, "scope": format!("openid {PLAN_SCOPE}")
        })))
        .mount(&server)
        .await;
    let http = reqwest::Client::new();
    let t = login::exchange_code(
        &http,
        &issuer(&server.uri()),
        &fixed_attempt(None),
        "oaiapp_9",
        "abc",
    )
    .await
    .unwrap();
    assert_eq!(t.access_token, "a");
    let body =
        String::from_utf8_lossy(&server.received_requests().await.unwrap()[0].body).to_string();
    assert!(!body.contains("client_secret"), "{body}");
}

#[tokio::test]
async fn a_spent_code_says_start_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})))
        .mount(&server)
        .await;
    let err = login::exchange_code(
        &reqwest::Client::new(),
        &issuer(&server.uri()),
        &fixed_attempt(None),
        "oaiapp_9",
        "abc",
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("start the sign-in again"), "{err}");
}

#[test]
fn the_id_token_is_verified_against_the_jwks() {
    let iss = "https://auth.openai.com";
    let good = id_token("oaiapp_9", "NONCE", 3600, "user-1");
    let c = login::validate_id_token(&good, &jwks(), iss, "oaiapp_9", "NONCE").unwrap();
    assert_eq!(c.sub, "user-1");
    assert_eq!(c.email.as_deref(), Some("u@example.com"));

    let err = |t: &str, aud: &str, nonce: &str| {
        login::validate_id_token(t, &jwks(), iss, aud, nonce)
            .unwrap_err()
            .to_string()
    };
    assert!(err(&good, "oaiapp_other", "NONCE").contains("different client"));
    assert!(err(&good, "oaiapp_9", "OTHER").contains("nonce"));
    assert!(err(
        &id_token("oaiapp_9", "NONCE", -3600, "user-1"),
        "oaiapp_9",
        "NONCE"
    )
    .contains("expired"));
    // Tampered payload: the signature no longer matches.
    let mut parts: Vec<String> = good.split('.').map(str::to_string).collect();
    parts[1] = parts[1].chars().rev().collect();
    assert!(login::validate_id_token(&parts.join("."), &jwks(), iss, "oaiapp_9", "NONCE").is_err());
    // A key the issuer does not publish.
    let empty: jsonwebtoken::jwk::JwkSet = serde_json::from_value(json!({"keys": []})).unwrap();
    assert!(login::validate_id_token(&good, &empty, iss, "oaiapp_9", "NONCE").is_err());
}

fn tokens(scope: &str) -> TokenResponse {
    serde_json::from_value(json!({"access_token": "a", "refresh_token": "r",
        "id_token": "i", "expires_in": 3600, "scope": scope}))
    .unwrap()
}

#[test]
fn a_returning_sign_in_must_be_the_same_account() {
    let claims = |sub: &str| -> login::IdClaims {
        serde_json::from_value(json!({"sub": sub, "email": "u@example.com"})).unwrap()
    };
    let p = prior();
    let ok = login::signed_in_record(
        Some(&p),
        "https://auth.openai.com",
        "oaiapp_123",
        "urn:uuid:h",
        &claims("user-1"),
        tokens(PLAN_SCOPE),
        1,
    )
    .unwrap();
    assert!(ok.plan_usage_enabled());
    assert_eq!(ok.client_id, "oaiapp_123");
    assert!(login::signed_in_record(
        Some(&p),
        "https://auth.openai.com",
        "oaiapp_123",
        "urn:uuid:h",
        &claims("someone-else"),
        tokens(PLAN_SCOPE),
        1,
    )
    .is_err());
}

#[test]
fn a_sign_in_without_the_plan_scope_is_kept_but_marked_disabled() {
    let claims: login::IdClaims = serde_json::from_value(json!({"sub": "u"})).unwrap();
    let rec = login::signed_in_record(
        None,
        "https://auth.openai.com",
        "oaiapp_9",
        "urn:uuid:h",
        &claims,
        tokens("openid email offline_access"),
        1,
    )
    .unwrap();
    assert!(rec.signed_in());
    assert!(!rec.plan_usage_enabled());
}

#[tokio::test]
async fn revocation_success_and_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/revoke"))
        .and(body_string_contains("token=r"))
        .and(body_string_contains("token_type_hint=refresh_token"))
        .and(body_string_contains("client_id=oaiapp_9"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let http = reqwest::Client::new();
    assert!(login::revoke(&http, &format!("{}/revoke", server.uri()), "oaiapp_9", "r").await);

    let down = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&down)
        .await;
    assert!(!login::revoke(&http, &format!("{}/revoke", down.uri()), "oaiapp_9", "r").await);
    assert_eq!(
        down.received_requests().await.unwrap().len(),
        3,
        "retried with backoff"
    );
}

#[tokio::test]
async fn discovery_supplies_the_jwks_and_revocation_endpoints() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": server.uri(), "jwks_uri": format!("{}/jwks", server.uri()),
            "revocation_endpoint": format!("{}/revoke", server.uri())
        })))
        .mount(&server)
        .await;
    let i = login::discover(&reqwest::Client::new(), &server.uri())
        .await
        .unwrap();
    assert_eq!(i.jwks_url, format!("{}/jwks", server.uri()));
    assert_eq!(
        i.authorize_url,
        format!("{}/api/accounts/authorize", server.uri())
    );
    assert_eq!(
        i.token_url,
        format!("{}/api/accounts/oauth/token", server.uri())
    );
}

/// The real issuer's discovery document and keys parse. Needs the network.
#[tokio::test]
#[ignore = "network: auth.openai.com"]
async fn the_real_issuer_publishes_parseable_keys() {
    let http = reqwest::Client::new();
    let i = login::discover(&http, cowboy_core::oauth::chatgpt::ISSUER)
        .await
        .unwrap();
    assert_eq!(
        i.authorize_url,
        "https://auth.openai.com/api/accounts/authorize"
    );
    assert!(i.revocation_url.is_some());
    let keys = login::fetch_jwks(&http, &i).await.unwrap();
    assert!(!keys.keys.is_empty());
    for k in &keys.keys {
        jsonwebtoken::DecodingKey::from_jwk(k).unwrap();
    }
}
