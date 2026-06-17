//! TCP client to the host `cowboy` process for "ask" decisions.
//!
//! The gateway connects to the host control server over TCP and authenticates with
//! a per-session token ([`GatewayMessage::Hello`], sent first). When the policy
//! yields `ask`, the gateway sends a [`GatewayMessage::Ask`] and blocks (with a
//! timeout) for a [`HostMessage::Decision`]. If the host is unavailable or auth
//! fails, asks fail closed (deny) — the host, not the agent, owns these decisions.

use std::time::Duration;

use anyhow::{Context, Result};
use cowboy_core::netproto::{encode_line, GatewayMessage, HostMessage, NetworkAttempt, Verdict};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

/// A connection to the host control server.
pub struct ControlClient {
    inner: Mutex<Option<Conn>>,
    addr: Option<String>,
    token: Option<String>,
    next_id: std::sync::atomic::AtomicU64,
}

struct Conn {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
}

impl ControlClient {
    pub fn new(addr: Option<String>, token: Option<String>) -> Self {
        Self {
            inner: Mutex::new(None),
            addr,
            token,
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Ask the host for a verdict. Fails closed (Deny) on any error or absence.
    /// `reason` (optional) explains why we're asking (shown in the host prompt).
    pub async fn ask(&self, attempt: &NetworkAttempt, reason: Option<&str>) -> Verdict {
        match self.ask_inner(attempt, reason).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, dest = %attempt.label(), "ask failed; denying (fail-closed)");
                Verdict::Deny
            }
        }
    }

    /// Connect to the host control server over TCP, retrying briefly to absorb
    /// startup races, then authenticate by sending `Hello { token }` first.
    async fn connect(addr: &str, token: Option<&str>) -> Result<TcpStream> {
        let mut last = None;
        for attempt in 0..20 {
            match TcpStream::connect(addr).await {
                Ok(mut s) => {
                    if attempt > 0 {
                        tracing::info!(%addr, attempt, "control server connected");
                    }
                    // Authenticate immediately (the host drops us otherwise).
                    let hello = GatewayMessage::Hello {
                        token: token.unwrap_or_default().to_string(),
                    };
                    s.write_all(encode_line(&hello).as_bytes()).await?;
                    s.flush().await?;
                    return Ok(s);
                }
                Err(e) => {
                    last = Some(e);
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            }
        }
        Err(last.unwrap()).with_context(|| format!("connecting control server {addr}"))
    }

    /// Ensure `guard` holds a live, authenticated connection, (re)connecting if absent.
    async fn ensure_conn<'a>(
        guard: &'a mut Option<Conn>,
        addr: &str,
        token: Option<&str>,
    ) -> Result<&'a mut Conn> {
        if guard.is_none() {
            let stream = Self::connect(addr, token).await?;
            let (r, w) = stream.into_split();
            *guard = Some(Conn {
                reader: BufReader::new(r),
                writer: w,
            });
        }
        Ok(guard.as_mut().unwrap())
    }

    async fn ask_inner(&self, attempt: &NetworkAttempt, reason: Option<&str>) -> Result<Verdict> {
        let Some(addr) = self.addr.clone() else {
            anyhow::bail!("no control address configured");
        };
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let mut guard = self.inner.lock().await;
        let result = Self::ask_on_conn(
            &mut guard,
            &addr,
            self.token.as_deref(),
            id,
            attempt,
            reason,
        )
        .await;
        if result.is_err() {
            // Drop a poisoned connection so the next call reconnects.
            *guard = None;
        }
        result
    }

    async fn ask_on_conn(
        guard: &mut Option<Conn>,
        addr: &str,
        token: Option<&str>,
        id: u64,
        attempt: &NetworkAttempt,
        reason: Option<&str>,
    ) -> Result<Verdict> {
        let conn = Self::ensure_conn(guard, addr, token).await?;

        let msg = GatewayMessage::Ask {
            id,
            reason: reason.map(str::to_string),
            attempt: attempt.clone(),
        };
        conn.writer.write_all(encode_line(&msg).as_bytes()).await?;
        conn.writer.flush().await?;

        // Read lines until we see the Decision for our id. Same budget as the
        // host worker's wait (shared const) so neither side gives up early.
        let deadline = Duration::from_secs(cowboy_core::netproto::APPROVAL_TIMEOUT_SECS);
        let verdict = tokio::time::timeout(deadline, async {
            let mut line = String::new();
            loop {
                line.clear();
                let n = conn.reader.read_line(&mut line).await?;
                if n == 0 {
                    anyhow::bail!("control socket closed");
                }
                if let Ok(HostMessage::Decision {
                    id: rid, verdict, ..
                }) = serde_json::from_str::<HostMessage>(line.trim())
                {
                    if rid == id {
                        return Ok::<Verdict, anyhow::Error>(verdict);
                    }
                }
            }
        })
        .await
        .context("timed out waiting for host decision")??;

        Ok(verdict)
    }

    /// Best-effort: notify the host of a decision for the activity log. Opens
    /// the connection if needed (so allow/deny verdicts — which never `ask` —
    /// still reach the host's activity pane).
    pub async fn event(&self, attempt: &NetworkAttempt, verdict: Verdict, reason: String) {
        let Some(addr) = self.addr.clone() else {
            return;
        };
        let mut guard = self.inner.lock().await;
        if let Err(e) = Self::event_on_conn(
            &mut guard,
            &addr,
            self.token.as_deref(),
            attempt,
            verdict,
            reason,
        )
        .await
        {
            tracing::debug!(error = %e, "control event send failed");
            *guard = None; // reconnect next time
        }
    }

    async fn event_on_conn(
        guard: &mut Option<Conn>,
        addr: &str,
        token: Option<&str>,
        attempt: &NetworkAttempt,
        verdict: Verdict,
        reason: String,
    ) -> Result<()> {
        let conn = Self::ensure_conn(guard, addr, token).await?;
        let msg = GatewayMessage::Event {
            attempt: attempt.clone(),
            verdict,
            reason,
        };
        conn.writer.write_all(encode_line(&msg).as_bytes()).await?;
        conn.writer.flush().await?;
        Ok(())
    }
}
