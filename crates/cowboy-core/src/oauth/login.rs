//! Sign in with ChatGPT: the authorization-code flow for open-source apps, minus
//! the loopback listener and browser (the CLI's). Everything here is either pure
//! or a single HTTP exchange, so each step can be tested on its own.
//!
//! The flow (OpenAI SIWC, "Registration and sign-in"):
//! 1. A first sign-in registers this installation with `client_id=dynamic_agent_client`,
//!    `agent_name_hint` and the host id; the callback returns the *issued* client id.
//!    Later sign-ins use that issued id (plus `id_token_hint`/`login_hint`).
//! 2. The code is exchanged with PKCE (no client secret) for tokens.
//! 3. The ID token is verified against OpenAI's JWKS — issuer, audience = issued
//!    client id, expiry, nonce — and, for a returning account, must be the same
//!    account (`sub`) before anything is replaced.
//! 4. Plan usage needs `chatgpt.tokens.use.direct` among the *granted* scopes.

use base64::Engine;
use serde::Deserialize;
use sha2::Digest;

use super::chatgpt::{ChatGptAuth, TokenResponse, PLAN_SCOPE, RESOURCE};
use crate::error::{Error, Result};

/// The authorize endpoint, relative to the issuer.
pub const AUTHORIZE_PATH: &str = "/api/accounts/authorize";
/// The first-registration entrypoint. Never saved as a client id.
pub const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
/// What the registration is called in the user's ChatGPT settings.
pub const AGENT_NAME: &str = "Cowboy";
/// Identity scopes plus the ChatGPT plan usage scopes.
pub const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// The only path the loopback redirect may use; only its port varies.
pub const CALLBACK_PATH: &str = "/auth/callback";

/// The issuer's endpoints. Authorize and token are documented fixed paths; the
/// JWKS and revocation endpoints come from discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issuer {
    pub issuer: String,
    pub authorize_url: String,
    pub token_url: String,
    pub jwks_url: String,
    pub revocation_url: Option<String>,
}

#[derive(Deserialize)]
struct Discovery {
    jwks_uri: String,
    #[serde(default)]
    revocation_endpoint: Option<String>,
}

/// Discover the issuer's JWKS and revocation endpoints.
pub async fn discover(http: &reqwest::Client, issuer: &str) -> Result<Issuer> {
    let base = issuer.trim_end_matches('/');
    let url = format!("{base}/.well-known/openid-configuration");
    let d: Discovery = http
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| Error::Model(format!("OpenID discovery at {url} failed: {e}")))?
        .json()
        .await
        .map_err(|e| Error::Model(format!("OpenID discovery at {url} was unreadable: {e}")))?;
    Ok(Issuer {
        issuer: base.to_string(),
        authorize_url: format!("{base}{AUTHORIZE_PATH}"),
        token_url: format!("{base}{}", super::chatgpt::TOKEN_PATH),
        jwks_url: d.jwks_uri,
        revocation_url: d.revocation_endpoint,
    })
}

/// One sign-in attempt's secrets: fresh for every attempt, kept only in memory.
#[derive(Clone)]
pub struct Attempt {
    pub state: String,
    pub nonce: String,
    pub verifier: String,
    pub redirect_uri: String,
    /// The issued client id for a returning account; `None` registers anew.
    pub client_id: Option<String>,
}

fn random_token() -> String {
    // Two v4 UUIDs: 244 random bits, already in PKCE's unreserved alphabet.
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

impl Attempt {
    pub fn new(redirect_uri: String, prior: Option<&ChatGptAuth>) -> Self {
        Self {
            state: random_token(),
            nonce: random_token(),
            verifier: random_token(),
            redirect_uri,
            client_id: prior
                .map(|p| p.client_id.clone())
                .filter(|c| !c.is_empty() && c != DYNAMIC_CLIENT),
        }
    }

    /// The PKCE `code_challenge`: base64url(SHA-256(verifier)), unpadded.
    pub fn challenge(&self) -> String {
        let digest = sha2::Sha256::digest(self.verifier.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
    }

    /// The browser URL. A returning account carries its issued client id and the
    /// retained hints; a first sign-in registers with the dynamic client and the
    /// app name. `consent` re-asks for plan usage after it was declined.
    ///
    /// The result may contain `id_token_hint`: show it to the user, never log it.
    pub fn authorize_url(
        &self,
        issuer: &Issuer,
        host_id: &str,
        prior: Option<&ChatGptAuth>,
        consent: bool,
    ) -> String {
        let mut q: Vec<(&str, String)> = vec![
            ("response_type", "code".into()),
            (
                "client_id",
                self.client_id
                    .clone()
                    .unwrap_or_else(|| DYNAMIC_CLIENT.to_string()),
            ),
            ("redirect_uri", self.redirect_uri.clone()),
            ("scope", SCOPES.into()),
            ("resource", RESOURCE.into()),
            ("state", self.state.clone()),
            ("nonce", self.nonce.clone()),
            ("code_challenge_method", "S256".into()),
            ("code_challenge", self.challenge()),
            ("ext_agent_host_id", host_id.into()),
        ];
        if self.client_id.is_none() {
            q.push(("agent_name_hint", AGENT_NAME.into()));
        } else if let Some(p) = prior {
            if let Some(h) = &p.id_token {
                q.push(("id_token_hint", h.clone()));
            }
            if let Some(e) = &p.email {
                q.push(("login_hint", e.clone()));
            }
        }
        if consent {
            q.push(("prompt", "consent".into()));
        }
        let mut url = reqwest::Url::parse(&issuer.authorize_url)
            .unwrap_or_else(|_| reqwest::Url::parse("https://auth.openai.com").unwrap());
        url.query_pairs_mut().extend_pairs(q);
        url.to_string()
    }
}

/// What came back to the loopback callback.
#[derive(Debug, PartialEq, Eq)]
pub enum Callback {
    Code {
        code: String,
        client_id: Option<String>,
    },
    /// The user declined.
    Denied,
}

/// Parse the callback's query string, checking `state` first — before anything
/// in it, including an error, is believed.
pub fn parse_callback(query: &str, expected_state: &str) -> Result<Callback> {
    let pairs: std::collections::BTreeMap<String, String> =
        reqwest::Url::parse(&format!("http://127.0.0.1/?{query}"))
            .map_err(|_| Error::Invalid("the sign-in callback was malformed".into()))?
            .query_pairs()
            .into_owned()
            .collect();
    if pairs.get("state").map(String::as_str) != Some(expected_state) {
        return Err(Error::Invalid(
            "the sign-in callback did not match this attempt (state mismatch); start again".into(),
        ));
    }
    if let Some(err) = pairs.get("error") {
        if err == "access_denied" {
            return Ok(Callback::Denied);
        }
        let desc = pairs
            .get("error_description")
            .map(|d| format!(": {d}"))
            .unwrap_or_default();
        return Err(Error::Invalid(format!("sign-in failed ({err}){desc}")));
    }
    let code = pairs
        .get("code")
        .filter(|c| !c.is_empty())
        .cloned()
        .ok_or_else(|| Error::Invalid("the sign-in callback carried no code".into()))?;
    Ok(Callback::Code {
        code,
        client_id: pairs.get("client_id").filter(|c| !c.is_empty()).cloned(),
    })
}

/// The client id to exchange with. A first registration must have been issued
/// one; a returning sign-in keeps its own and refuses a different one rather than
/// let a callback swap the account's registration.
pub fn issued_client_id(attempt: &Attempt, callback: Option<&str>) -> Result<String> {
    match (&attempt.client_id, callback) {
        (None, Some(c)) if c != DYNAMIC_CLIENT => Ok(c.to_string()),
        (None, _) => Err(Error::Invalid(
            "registration did not complete: the callback carried no issued client id; \
             start the sign-in again"
                .into(),
        )),
        (Some(ours), None) => Ok(ours.clone()),
        (Some(ours), Some(c)) if c == ours => Ok(ours.clone()),
        (Some(_), Some(_)) => Err(Error::Invalid(
            "the sign-in callback named a different client registration than this account's; \
             refusing to replace it"
                .into(),
        )),
    }
}

/// Exchange the authorization code (PKCE, no secret) for tokens. `invalid_grant`
/// means the code is spent or expired: the caller starts over.
pub async fn exchange_code(
    http: &reqwest::Client,
    issuer: &Issuer,
    attempt: &Attempt,
    client_id: &str,
    code: &str,
) -> Result<TokenResponse> {
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("code", code),
        ("code_verifier", attempt.verifier.as_str()),
        ("redirect_uri", attempt.redirect_uri.as_str()),
        ("resource", RESOURCE),
    ];
    let resp = http
        .post(&issuer.token_url)
        .form(&form)
        .send()
        .await
        .map_err(|e| Error::Model(format!("the token exchange failed: {e}")))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        return serde_json::from_str(&body)
            .map_err(|_| Error::Model("the token endpoint's response was unreadable".into()));
    }
    Err(match super::chatgpt::error_code(&body).as_deref() {
        Some("invalid_grant") => Error::Invalid(
            "the sign-in code expired or was already used; start the sign-in again".into(),
        ),
        Some(c) => Error::Model(format!("the token exchange was refused ({status}, {c})")),
        None => Error::Model(format!("the token exchange was refused ({status})")),
    })
}

/// The verified ID-token claims Cowboy keeps.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IdClaims {
    pub sub: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
}

/// Fetch the issuer's signing keys.
pub async fn fetch_jwks(
    http: &reqwest::Client,
    issuer: &Issuer,
) -> Result<jsonwebtoken::jwk::JwkSet> {
    http.get(&issuer.jwks_url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| Error::Model(format!("fetching the issuer's signing keys failed: {e}")))?
        .json()
        .await
        .map_err(|e| Error::Model(format!("the issuer's signing keys were unreadable: {e}")))
}

/// Verify the ID token's signature against `jwks` and its issuer, audience
/// (the issued client id), expiry and nonce.
pub fn validate_id_token(
    id_token: &str,
    jwks: &jsonwebtoken::jwk::JwkSet,
    issuer: &str,
    client_id: &str,
    nonce: &str,
) -> Result<IdClaims> {
    use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
    let bad = |why: &str| Error::Invalid(format!("the ID token failed verification: {why}"));
    let header = decode_header(id_token).map_err(|_| bad("not a JWT"))?;
    let jwk = match &header.kid {
        Some(kid) => jwks.find(kid),
        None if jwks.keys.len() == 1 => jwks.keys.first(),
        None => None,
    }
    .ok_or_else(|| bad("signed with a key the issuer does not publish"))?;
    let key = DecodingKey::from_jwk(jwk).map_err(|_| bad("unusable signing key"))?;
    if !matches!(
        header.alg,
        Algorithm::RS256 | Algorithm::ES256 | Algorithm::PS256
    ) {
        return Err(bad("unexpected signing algorithm"));
    }
    let mut v = Validation::new(header.alg);
    v.set_issuer(&[
        issuer.trim_end_matches('/'),
        &format!("{}/", issuer.trim_end_matches('/')),
    ]);
    v.set_audience(&[client_id]);
    let data = decode::<IdClaims>(id_token, &key, &v).map_err(|e| {
        use jsonwebtoken::errors::ErrorKind as K;
        bad(match e.kind() {
            K::ExpiredSignature => "expired",
            K::InvalidAudience => "issued for a different client",
            K::InvalidIssuer => "from a different issuer",
            K::InvalidSignature => "bad signature",
            _ => "invalid",
        })
    })?;
    if data.claims.nonce.as_deref() != Some(nonce) {
        return Err(bad("nonce does not match this sign-in"));
    }
    Ok(data.claims)
}

/// The record to save after a verified sign-in. A returning account must be the
/// same account (`sub`) as the record it replaces.
pub fn signed_in_record(
    prior: Option<&ChatGptAuth>,
    issuer: &str,
    client_id: &str,
    host_id: &str,
    claims: &IdClaims,
    tokens: TokenResponse,
    now: u64,
) -> Result<ChatGptAuth> {
    if let Some(p) = prior {
        let same_client = !p.client_id.is_empty() && p.client_id == client_id;
        if let (true, Some(sub)) = (same_client, &p.subject) {
            if sub != &claims.sub {
                return Err(Error::Invalid(
                    "signed in as a different ChatGPT account than this provider's; sign in with \
                     the original account, or add a separate provider for the other one"
                        .into(),
                ));
            }
        }
    }
    let mut rec = ChatGptAuth {
        issuer: issuer.trim_end_matches('/').to_string(),
        client_id: client_id.to_string(),
        ext_agent_host_id: host_id.to_string(),
        subject: Some(claims.sub.clone()),
        email: claims.email.clone(),
        ..Default::default()
    };
    rec.apply_tokens(tokens, now);
    Ok(rec)
}

/// Whether a token set grants plan usage.
pub fn grants_plan_usage(rec: &ChatGptAuth) -> bool {
    rec.scopes.iter().any(|s| s == PLAN_SCOPE)
}

/// End the renewable session: revoke the refresh token. `Ok(true)` when the issuer
/// confirmed (an empty 200, including for an already-invalid token). Network
/// errors and 5xx are retried with backoff; `Ok(false)` when it never confirmed.
pub async fn revoke(
    http: &reqwest::Client,
    revocation_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> bool {
    let form = [
        ("token", refresh_token),
        ("token_type_hint", "refresh_token"),
        ("client_id", client_id),
    ];
    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(
                500 * 2u64.pow(attempt - 1),
            ))
            .await;
        }
        match http.post(revocation_url).form(&form).send().await {
            Ok(r) if r.status().is_success() => return true,
            Ok(r) if r.status().is_server_error() => continue,
            Ok(_) => return false,
            Err(_) => continue,
        }
    }
    false
}
