//! Unix-socket client to the host `cowboy` process for "ask" decisions.
//!
//! The gateway connects to the host-owned socket. When the policy yields `ask`,
//! the gateway sends a [`GatewayMessage::Ask`] and blocks (with a timeout) for a
//! [`HostMessage::Decision`]. If the socket is unavailable, asks fail closed
//! (deny) — the host, not the agent, owns these decisions.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use cowboy_core::netproto::{encode_line, GatewayMessage, HostMessage, NetworkAttempt, Verdict};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

/// A connection to the host control socket.
pub struct ControlClient {
    inner: Mutex<Option<Conn>>,
    path: Option<PathBuf>,
    next_id: std::sync::atomic::AtomicU64,
}

struct Conn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl ControlClient {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            inner: Mutex::new(None),
            path,
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Ask the host for a verdict. Fails closed (Deny) on any error or absence.
    pub async fn ask(&self, attempt: &NetworkAttempt) -> Verdict {
        match self.ask_inner(attempt).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, dest = %attempt.label(), "ask failed; denying (fail-closed)");
                Verdict::Deny
            }
        }
    }

    /// Connect to the host socket, retrying briefly to absorb startup races
    /// (the host may bind the socket slightly after the gateway comes up).
    async fn connect(path: &std::path::Path) -> Result<UnixStream> {
        let mut last = None;
        for attempt in 0..20 {
            match UnixStream::connect(path).await {
                Ok(s) => {
                    if attempt > 0 {
                        tracing::info!(sock = %path.display(), attempt, "control socket connected");
                    }
                    return Ok(s);
                }
                Err(e) => {
                    last = Some(e);
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            }
        }
        Err(last.unwrap()).with_context(|| format!("connecting control socket {}", path.display()))
    }

    /// Ensure `guard` holds a live connection, (re)connecting if absent.
    async fn ensure_conn<'a>(
        guard: &'a mut Option<Conn>,
        path: &std::path::Path,
    ) -> Result<&'a mut Conn> {
        if guard.is_none() {
            let stream = Self::connect(path).await?;
            let (r, w) = stream.into_split();
            *guard = Some(Conn {
                reader: BufReader::new(r),
                writer: w,
            });
        }
        Ok(guard.as_mut().unwrap())
    }

    async fn ask_inner(&self, attempt: &NetworkAttempt) -> Result<Verdict> {
        let Some(path) = self.path.clone() else {
            anyhow::bail!("no control socket configured");
        };
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let mut guard = self.inner.lock().await;
        let result = Self::ask_on_conn(&mut guard, &path, id, attempt).await;
        if result.is_err() {
            // Drop a poisoned connection so the next call reconnects.
            *guard = None;
        }
        result
    }

    async fn ask_on_conn(
        guard: &mut Option<Conn>,
        path: &std::path::Path,
        id: u64,
        attempt: &NetworkAttempt,
    ) -> Result<Verdict> {
        let conn = Self::ensure_conn(guard, path).await?;

        let msg = GatewayMessage::Ask {
            id,
            attempt: attempt.clone(),
        };
        conn.writer.write_all(encode_line(&msg).as_bytes()).await?;
        conn.writer.flush().await?;

        // Read lines until we see the Decision for our id (120s budget).
        let deadline = Duration::from_secs(120);
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
        let Some(path) = self.path.clone() else {
            return;
        };
        let mut guard = self.inner.lock().await;
        if let Err(e) = Self::event_on_conn(&mut guard, &path, attempt, verdict, reason).await {
            tracing::debug!(error = %e, "control event send failed");
            *guard = None; // reconnect next time
        }
    }

    async fn event_on_conn(
        guard: &mut Option<Conn>,
        path: &std::path::Path,
        attempt: &NetworkAttempt,
        verdict: Verdict,
        reason: String,
    ) -> Result<()> {
        let conn = Self::ensure_conn(guard, path).await?;
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
