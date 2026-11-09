//! What every HTTP model client shares, whatever its wire API: where the bearer
//! token comes from, sending a request with backoff, and reading an SSE stream
//! that may stall.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;

use super::{backoff, is_model_not_found, is_retryable_status, is_transient, oa_err, retry_after};
use super::{MAX_RETRIES, MAX_WAIT};
use crate::error::{Error, Result};

/// Where a request's bearer token comes from. A static key never changes; an
/// OAuth source refreshes, so the token is asked for per request rather than
/// baked into the client.
#[async_trait]
pub trait TokenSource: Send + Sync {
    /// The token to send now. Empty means send no `Authorization` header.
    async fn token(&self) -> Result<String>;

    /// The provider refused `stale` with a 401. Return `Ok(true)` when a fresh
    /// token is now available and the request is worth one retry.
    async fn on_unauthorized(&self, _stale: &str) -> Result<bool> {
        Ok(false)
    }
}

/// A fixed API key (or none, for a keyless gateway).
pub struct StaticKey(pub String);

#[async_trait]
impl TokenSource for StaticKey {
    async fn token(&self) -> Result<String> {
        Ok(self.0.clone())
    }
}

/// Turns a failed response into the error the caller sees. Wire APIs differ in
/// what their error bodies mean (a ChatGPT-plan usage limit is not a rate limit
/// to back off from), so the client supplies this.
pub trait FailurePolicy: Send + Sync {
    /// Whether a status is worth retrying with backoff.
    fn retryable(&self, status: reqwest::StatusCode, _body: &str) -> bool {
        is_retryable_status(status)
    }

    /// The error for a response that will not be retried.
    fn error(&self, status: reqwest::StatusCode, body: &str, request_id: Option<&str>) -> Error {
        default_error(status, body, request_id)
    }

    /// The error for a failure reported inside an already-open stream
    /// (`response.failed`, an `error` event).
    fn stream_error(
        &self,
        code: Option<&str>,
        message: Option<&str>,
        param: Option<&str>,
    ) -> Error {
        super::responses::stream_error(code, message, param)
    }
}

/// The policy for ordinary API-key endpoints.
pub struct DefaultFailures;
impl FailurePolicy for DefaultFailures {}

/// `model request failed (…)` — or [`Error::ModelUnavailable`] when the body names
/// a missing model, so the loop can reroute instead of failing the session.
pub fn default_error(status: reqwest::StatusCode, body: &str, request_id: Option<&str>) -> Error {
    let rid = request_id
        .map(|r| format!(" [request {r}]"))
        .unwrap_or_default();
    let msg = format!("model request failed ({status}){rid}: {body}");
    if is_model_not_found(status, body) {
        Error::ModelUnavailable(msg)
    } else {
        Error::Model(msg)
    }
}

/// The provider's request id, for error reports.
pub fn request_id(resp: &reqwest::Response) -> Option<String> {
    ["x-request-id", "openai-request-id", "request-id"]
        .iter()
        .find_map(|h| resp.headers().get(*h)?.to_str().ok().map(str::to_string))
}

/// POST `body` to `url` with the source's bearer token, retrying rate limits,
/// overload, 5xx and transient connection errors with backoff (streaming has not
/// started, so a retry is clean), and refreshing once on a 401 when the source
/// can. Returns the successful response, unread.
pub async fn send_with_retry(
    http: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    auth: &dyn TokenSource,
    policy: &dyn FailurePolicy,
) -> Result<reqwest::Response> {
    let mut attempt = 0u32;
    let mut refreshed = false;
    loop {
        let token = auth.token().await?;
        let mut req = http.post(url).json(body);
        if !token.is_empty() {
            req = req.bearer_auth(&token);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => return Ok(r),
            Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED && !refreshed => {
                // One refresh-and-retry: a token that expired between being handed
                // out and arriving is ordinary for OAuth, and says nothing about the
                // credentials. A second 401 is reported.
                refreshed = true;
                if auth.on_unauthorized(&token).await? {
                    continue;
                }
                let rid = request_id(&r);
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                return Err(policy.error(status, &text, rid.as_deref()));
            }
            Ok(r) => {
                let status = r.status();
                let rid = request_id(&r);
                let delay = retry_after(&r);
                let text = r.text().await.unwrap_or_default();
                if attempt < MAX_RETRIES && policy.retryable(status, &text) {
                    // A provider signal of 0 (`Retry-After: 0`, or a reset header that
                    // already elapsed) is not a licence to retry instantly and re-hammer
                    // the endpoint — treat a zero/absent signal as "use our jittered
                    // backoff", which also de-synchronizes parallel retriers.
                    let delay = delay
                        .filter(|d| !d.is_zero())
                        .unwrap_or_else(|| backoff(attempt))
                        .min(MAX_WAIT);
                    tracing::warn!(
                        status = %status,
                        attempt = attempt + 1,
                        delay_ms = delay.as_millis() as u64,
                        "model request throttled/failed; backing off and retrying"
                    );
                    attempt += 1;
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(policy.error(status, &text, rid.as_deref()));
            }
            Err(e) if is_transient(&e) && attempt < MAX_RETRIES => {
                let delay = backoff(attempt);
                tracing::warn!(
                    error = %e,
                    attempt = attempt + 1,
                    delay_ms = delay.as_millis() as u64,
                    "model request connection error; backing off and retrying"
                );
                attempt += 1;
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(oa_err(e)),
        }
    }
}

/// A server-sent-event stream read one `data:` payload at a time, bounded by an
/// idle timeout.
pub struct SseStream {
    stream: futures::stream::BoxStream<'static, reqwest::Result<Vec<u8>>>,
    buf: String,
    idle: Duration,
    ended: bool,
}

impl SseStream {
    pub fn new(resp: reqwest::Response, idle: Duration) -> Self {
        Self {
            stream: resp.bytes_stream().map(|r| r.map(|b| b.to_vec())).boxed(),
            buf: String::new(),
            idle,
            ended: false,
        }
    }

    /// The next non-empty `data:` payload (trimmed), or `None` at end of stream.
    /// Comments, `event:` lines and blank keep-alives are skipped.
    pub async fn next_data(&mut self) -> Result<Option<String>> {
        loop {
            // SSE: process complete `\n`-terminated lines, leaving any partial line
            // in the buffer for the next chunk.
            while let Some(nl) = self.buf.find('\n') {
                let line: String = self.buf.drain(..=nl).collect();
                let Some(data) = line.trim().strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if !data.is_empty() {
                    return Ok(Some(data.to_string()));
                }
            }
            if self.ended {
                return Ok(None);
            }
            // Bound the wait for the next frame: a provider whose stream stalls
            // silently mid-response (observed with wafer/MiniMax-M3: reasoning
            // deltas, then nothing, forever) must error out, not hang the turn.
            // Any bytes on the wire — including SSE keep-alive comments — reset
            // the clock, so slow-but-alive streams are unaffected.
            let item = if self.idle.is_zero() {
                self.stream.next().await
            } else {
                match tokio::time::timeout(self.idle, self.stream.next()).await {
                    Ok(item) => item,
                    Err(_) => {
                        return Err(Error::Model(format!(
                            "model stream stalled: no data from the provider for {}s \
                             (tune with `stream_idle_timeout_seconds` on the model; 0 disables)",
                            self.idle.as_secs()
                        )))
                    }
                }
            };
            match item {
                Some(chunk) => self
                    .buf
                    .push_str(&String::from_utf8_lossy(&chunk.map_err(oa_err)?)),
                None => {
                    // End of stream. Anything left in the buffer is a frame with no
                    // trailing newline, which the line loop would never see — so a
                    // provider that ends without a final `\n` silently lost its last
                    // event, often the one carrying the finish reason or the tail of
                    // a tool call. Terminate the partial line and make one final pass.
                    self.ended = true;
                    if !self.buf.trim().is_empty() {
                        self.buf.push('\n');
                    }
                }
            }
        }
    }
}
