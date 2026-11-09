//! Sign in with ChatGPT: the credential record and its refresh.
//!
//! Follows OpenAI's flow for open-source apps (developers.openai.com/siwc). Each
//! provider with `auth: chatgpt` has one record, `auth/<provider>.yaml` (0600,
//! written atomically): the registration's issued `client_id`, the account it is
//! bound to, and the current token set.
//!
//! Access tokens last an hour; each refresh returns a *replacement* refresh token
//! and invalidates the old one. Subagents are separate processes sharing the
//! record, so a refresh is serialized across processes with an `flock` on
//! `auth/<provider>.lock`, and every process re-reads the record under the lock
//! before refreshing — the second one in finds the first one's new token instead
//! of spending a refresh token that has just been rotated away.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::TokenSource;
use crate::time::now_ms;

/// The production issuer.
pub const ISSUER: &str = "https://auth.openai.com";
/// The token endpoint, relative to the issuer.
pub const TOKEN_PATH: &str = "/api/accounts/oauth/token";
/// The resource every ChatGPT-plan token is for (sent on exchange and refresh).
pub const RESOURCE: &str = "https://api.openai.com/v1";
/// The scope that authorizes inference on the user's ChatGPT plan.
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";

/// Refresh this long before the access token's expiry, so a request never
/// leaves with a token that dies in flight.
const REFRESH_MARGIN_MS: u64 = 2 * 60 * 1000;
/// Refresh tokens last 30 days from issue.
pub const REFRESH_TOKEN_LIFETIME_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// One ChatGPT registration and its tokens. Fields follow OpenAI's documented
/// local credential record; times are Cowboy's `u64` epoch milliseconds.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatGptAuth {
    pub issuer: String,
    /// The issued client id (`oaiapp_…`) — never `dynamic_agent_client`.
    pub client_id: String,
    pub ext_agent_host_id: String,
    /// The validated ID-token subject: the account identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Retained for `id_token_hint` on the next sign-in, even after sign-out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    pub expires_in: u64,
    /// When the access token expires.
    pub expires_at_ms: u64,
    /// When the current token set was received (the refresh token's issue time).
    pub saved_at_ms: u64,
    /// Not before this, per the token response's `earliest_refresh_at`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub earliest_refresh_at_ms: Option<u64>,
    pub scopes: Vec<String>,
}

/// Never print a token, even in a debug dump.
impl std::fmt::Debug for ChatGptAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = |o: &Option<String>| if o.is_some() { "<set>" } else { "none" };
        f.debug_struct("ChatGptAuth")
            .field("client_id", &self.client_id)
            .field("email", &self.email)
            .field("access_token", &set(&self.access_token))
            .field("refresh_token", &set(&self.refresh_token))
            .field("expires_at_ms", &self.expires_at_ms)
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl ChatGptAuth {
    /// Whether the granted scopes allow inference on the ChatGPT plan. A valid
    /// sign-in without it is kept, but cannot be used for requests.
    pub fn plan_usage_enabled(&self) -> bool {
        self.scopes.iter().any(|s| s == PLAN_SCOPE)
    }

    /// Signed in: there is a token set to use or refresh.
    pub fn signed_in(&self) -> bool {
        self.refresh_token.is_some() || self.access_token.is_some()
    }

    /// When the refresh token itself runs out, if nothing refreshes it first.
    pub fn refresh_token_expires_at_ms(&self) -> Option<u64> {
        self.refresh_token
            .as_ref()
            .map(|_| self.saved_at_ms + REFRESH_TOKEN_LIFETIME_MS)
    }

    fn fresh_access(&self, now: u64) -> Option<&str> {
        self.access_token
            .as_deref()
            .filter(|_| self.expires_at_ms > now + REFRESH_MARGIN_MS)
    }

    /// Fold a successful token response in: the access token, its expiry, the
    /// scopes and the rotated refresh token are replaced together, never piecemeal.
    pub fn apply_tokens(&mut self, t: TokenResponse, now: u64) {
        self.access_token = Some(t.access_token);
        if let Some(r) = t.refresh_token {
            self.refresh_token = Some(r);
        }
        if let Some(i) = t.id_token {
            self.id_token = Some(i);
        }
        if let Some(tt) = t.token_type {
            self.token_type = Some(tt);
        }
        self.expires_in = t.expires_in.unwrap_or(3600);
        self.expires_at_ms = now + self.expires_in * 1000;
        if let Some(scope) = t.scope {
            self.scopes = scope.split_whitespace().map(str::to_string).collect();
        }
        self.earliest_refresh_at_ms = t.earliest_refresh_at.as_ref().and_then(timestamp_ms);
        self.saved_at_ms = now;
    }

    /// Drop the token set but keep the registration (client id, account, retained
    /// id token for the hint) so the next sign-in reuses it.
    pub fn clear_tokens(&mut self) {
        self.access_token = None;
        self.refresh_token = None;
        self.expires_at_ms = 0;
        self.earliest_refresh_at_ms = None;
    }
}

/// The token endpoint's success body (code exchange and refresh alike).
#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub earliest_refresh_at: Option<serde_json::Value>,
}

/// `earliest_refresh_at` as epoch ms: seconds, milliseconds, or an RFC 3339 string.
fn timestamp_ms(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => {
            let n = n.as_f64()?;
            Some(if n > 1e12 {
                n as u64
            } else {
                (n * 1000.0) as u64
            })
        }
        serde_json::Value::String(s) => s
            .parse::<jiff::Timestamp>()
            .ok()
            .map(|t| t.as_millisecond().max(0) as u64),
        _ => None,
    }
}

/// `auth/<provider>.yaml`.
pub fn record_path(dir: &Path, provider: &str) -> PathBuf {
    dir.join(format!("{provider}.yaml"))
}

fn lock_path(dir: &Path, provider: &str) -> PathBuf {
    dir.join(format!("{provider}.lock"))
}

/// Read a provider's record, or `None` when it has none yet.
pub fn load(dir: &Path, provider: &str) -> Result<Option<ChatGptAuth>> {
    let path = record_path(dir, provider);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_yaml_ng::from_str(&text)
            .map(Some)
            .map_err(|source| Error::ConfigParse { path, source }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::ConfigRead { path, source }),
    }
}

/// Write a provider's record atomically, owner-only.
pub fn save(dir: &Path, provider: &str, rec: &ChatGptAuth) -> Result<()> {
    crate::config::write_yaml_private(rec, &record_path(dir, provider))
}

/// An exclusive advisory lock on a provider's record, released on drop.
pub struct RecordLock(#[allow(dead_code)] std::fs::File);

/// Take the cross-process lock for a provider's record (blocking).
pub fn lock(dir: &Path, provider: &str) -> Result<RecordLock> {
    std::fs::create_dir_all(dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(dir, provider))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a file descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(RecordLock(file))
}

/// Refresh-error codes after which the token set is unusable and only signing in
/// again helps (OpenAI SIWC "Refresh errors").
const TERMINAL: &[&str] = &[
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

/// The error code in a token-endpoint error body: `{"error": "code"}` or
/// `{"error": {"code": "…"}}`.
pub fn error_code(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    match &v["error"] {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(o) => o
            .get("code")
            .or_else(|| o.get("type"))
            .and_then(|c| c.as_str())
            .map(str::to_string),
        _ => v["code"].as_str().map(str::to_string),
    }
}

/// Where a [`ChatGptTokens`] talks to: the issuer (overridable for tests).
#[derive(Clone)]
pub struct Endpoints {
    pub token_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            token_url: format!("{ISSUER}{TOKEN_PATH}"),
        }
    }
}

/// The token source for an `auth: chatgpt` provider.
pub struct ChatGptTokens {
    provider: String,
    dir: PathBuf,
    endpoints: Endpoints,
    http: reqwest::Client,
    /// The last token handed out and its expiry, so ordinary requests do not
    /// re-read the record.
    cache: tokio::sync::Mutex<Option<(String, u64)>>,
}

impl ChatGptTokens {
    pub fn new(provider: &str, dir: PathBuf, endpoints: Endpoints) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Model(e.to_string()))?;
        Ok(Self {
            provider: provider.to_string(),
            dir,
            endpoints,
            http,
            cache: tokio::sync::Mutex::new(None),
        })
    }

    /// For the production auth dir and issuer.
    pub fn for_provider(provider: &str) -> Result<Arc<Self>> {
        Ok(Arc::new(Self::new(
            provider,
            super::auth_dir()?,
            Endpoints::default(),
        )?))
    }

    fn sign_in_again(&self, why: &str) -> Error {
        Error::SignInRequired(format!(
            "ChatGPT sign-in for provider `{p}` {why}; run `cowboy providers login {p}`",
            p = self.provider
        ))
    }

    /// A usable access token, refreshing under the cross-process lock when the
    /// stored one is (nearly) expired — or unconditionally when `force` is set and
    /// the stored token is still `stale` (the provider refused it).
    async fn get(&self, force_past: Option<&str>) -> Result<String> {
        let now = now_ms();
        if force_past.is_none() {
            if let Some((tok, exp)) = self.cache.lock().await.as_ref() {
                if *exp > now + REFRESH_MARGIN_MS {
                    return Ok(tok.clone());
                }
            }
        }
        let dir = self.dir.clone();
        let provider = self.provider.clone();
        let _lock = tokio::task::spawn_blocking(move || lock(&dir, &provider))
            .await
            .map_err(|e| Error::Model(e.to_string()))??;

        // Re-read under the lock: another process may have just refreshed.
        let mut rec = load(&self.dir, &self.provider)?
            .ok_or_else(|| self.sign_in_again("has no saved sign-in"))?;
        if !rec.plan_usage_enabled() && rec.signed_in() {
            return Err(Error::SignInRequired(format!(
                "ChatGPT sign-in for provider `{p}` does not allow using your ChatGPT plan \
                 (consent was not granted); run `cowboy providers login {p}` to enable it",
                p = self.provider
            )));
        }
        let now = now_ms();
        let usable = rec.fresh_access(now).map(str::to_string);
        let someone_refreshed = match (force_past, rec.access_token.as_deref()) {
            (Some(stale), Some(cur)) => cur != stale,
            _ => false,
        };
        if let Some(tok) = &usable {
            if force_past.is_none() || someone_refreshed {
                *self.cache.lock().await = Some((tok.clone(), rec.expires_at_ms));
                return Ok(tok.clone());
            }
        }
        // `earliest_refresh_at`: while the current token still works, wait.
        if let (Some(earliest), Some(tok)) = (rec.earliest_refresh_at_ms, &rec.access_token) {
            if now < earliest && rec.expires_at_ms > now && force_past.is_none() {
                return Ok(tok.clone());
            }
        }
        let Some(refresh) = rec.refresh_token.clone() else {
            return Err(self.sign_in_again("has expired"));
        };
        let t = self.refresh(&rec.client_id, &refresh).await;
        match t {
            Ok(t) => {
                rec.apply_tokens(t, now_ms());
                save(&self.dir, &self.provider, &rec)?;
                let tok = rec.access_token.clone().unwrap_or_default();
                *self.cache.lock().await = Some((tok.clone(), rec.expires_at_ms));
                Ok(tok)
            }
            Err(RefreshError::Terminal(code)) => {
                // Unusable for good: clear it (keeping the registration) so nothing
                // keeps retrying a dead token, and say what fixes it.
                rec.clear_tokens();
                save(&self.dir, &self.provider, &rec)?;
                *self.cache.lock().await = None;
                Err(self.sign_in_again(&format!("has expired or was revoked ({code})")))
            }
            Err(RefreshError::InvalidClient) => Err(Error::Invalid(format!(
                "ChatGPT sign-in for provider `{p}`: the token endpoint rejected the client \
                 registration (invalid_client); sign in again with `cowboy providers login {p}`",
                p = self.provider
            ))),
            Err(RefreshError::Transient(msg)) => Err(Error::Model(format!(
                "could not refresh the ChatGPT sign-in for provider `{}` ({msg}); the saved \
                 sign-in is kept — retry shortly",
                self.provider
            ))),
        }
    }

    /// One `grant_type=refresh_token` exchange, retried with backoff on network
    /// errors and 5xx. Never sends `scope` (omitting it keeps the grant).
    async fn refresh(
        &self,
        client_id: &str,
        refresh_token: &str,
    ) -> std::result::Result<TokenResponse, RefreshError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("resource", RESOURCE),
        ];
        let mut last = String::new();
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt - 1))).await;
            }
            let resp = match self
                .http
                .post(&self.endpoints.token_url)
                .form(&form)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last = if e.is_timeout() {
                        "timed out".into()
                    } else {
                        "network error".into()
                    };
                    continue;
                }
            };
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if status.is_success() {
                return serde_json::from_str(&body)
                    .map_err(|_| RefreshError::Transient("unreadable token response".into()));
            }
            let code = error_code(&body);
            match code.as_deref() {
                Some(c) if TERMINAL.contains(&c) => {
                    return Err(RefreshError::Terminal(c.to_string()))
                }
                Some("invalid_client") => return Err(RefreshError::InvalidClient),
                _ if status.is_server_error() || status.as_u16() == 429 => {
                    last = format!("HTTP {status}");
                }
                // A 4xx without a recognised code is not going to change on retry.
                _ => {
                    return Err(RefreshError::Transient(format!(
                        "HTTP {status}{}",
                        code.map(|c| format!(", {c}")).unwrap_or_default()
                    )))
                }
            }
        }
        Err(RefreshError::Transient(last))
    }
}

enum RefreshError {
    Terminal(String),
    InvalidClient,
    Transient(String),
}

#[async_trait]
impl TokenSource for ChatGptTokens {
    async fn token(&self) -> Result<String> {
        self.get(None).await
    }

    async fn on_unauthorized(&self, stale: &str) -> Result<bool> {
        *self.cache.lock().await = None;
        self.get(Some(stale)).await.map(|_| true)
    }
}
