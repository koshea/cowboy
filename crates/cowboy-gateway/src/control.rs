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

/// Connect retries for an `ask` (a real decision the host owns): retry ~3 s to
/// absorb the host control server's startup race.
const ASK_CONNECT_ATTEMPTS: usize = 20;

/// Best-effort decision logging must never stall egress. An allowed connection
/// only reaches the splice after `decide` returns, so a slow `event` directly
/// delays the user's traffic. Cap the whole send (single connect + write) so a
/// missing/unreachable host costs milliseconds, not seconds.
const EVENT_DEADLINE: Duration = Duration::from_millis(250);

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

    /// Connect to the host control server over TCP, making up to `attempts`
    /// tries (150 ms apart) to absorb startup races, then authenticate by sending
    /// `Hello { token }` first. `attempts == 1` is a single fast try (no retry
    /// sleeps) used by best-effort event logging, which must never stall egress.
    async fn connect(addr: &str, token: Option<&str>, attempts: usize) -> Result<TcpStream> {
        let mut last = None;
        for attempt in 0..attempts.max(1) {
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
                    if attempt + 1 < attempts {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                    }
                }
            }
        }
        Err(last.unwrap()).with_context(|| format!("connecting control server {addr}"))
    }

    /// Ensure `guard` holds a live, authenticated connection, (re)connecting (with
    /// up to `attempts` tries) if absent.
    async fn ensure_conn<'a>(
        guard: &'a mut Option<Conn>,
        addr: &str,
        token: Option<&str>,
        attempts: usize,
    ) -> Result<&'a mut Conn> {
        if guard.is_none() {
            let stream = Self::connect(addr, token, attempts).await?;
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
        let conn = Self::ensure_conn(guard, addr, token, ASK_CONNECT_ATTEMPTS).await?;

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
    ///
    /// This runs on the egress critical path (an allowed connection splices only
    /// after `decide` returns), so it is strictly bounded: a single connect try,
    /// the whole thing capped by [`EVENT_DEADLINE`]. A missing or unreachable
    /// host drops the log line — never stalls the user's traffic.
    pub async fn event(&self, attempt: &NetworkAttempt, verdict: Verdict, reason: String) {
        let Some(addr) = self.addr.clone() else {
            return;
        };
        let mut guard = self.inner.lock().await;
        let send = Self::event_on_conn(
            &mut guard,
            &addr,
            self.token.as_deref(),
            attempt,
            verdict,
            reason,
        );
        match tokio::time::timeout(EVENT_DEADLINE, send).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "control event send failed");
                *guard = None; // reconnect next time
            }
            Err(_) => {
                tracing::debug!("control event timed out (best-effort; dropped)");
                *guard = None;
            }
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
        // Single fast connect attempt: best-effort logging must not retry-storm.
        let conn = Self::ensure_conn(guard, addr, token, 1).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::netproto::{ApprovalScope, Protocol};
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::TcpListener;

    fn attempt() -> NetworkAttempt {
        NetworkAttempt {
            protocol: Protocol::Tls,
            host: Some("example.com".into()),
            ip: None,
            port: 443,
        }
    }

    #[tokio::test]
    async fn no_control_addr_fails_closed() {
        // No host to ask → deny, never allow.
        let c = ControlClient::new(None, None);
        assert_eq!(c.ask(&attempt(), None).await, Verdict::Deny);
    }

    #[tokio::test]
    async fn ask_skips_unmatched_id_and_returns_decision() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut br = BufReader::new(r);
            let mut line = String::new();
            br.read_line(&mut line).await.unwrap(); // Hello
            line.clear();
            br.read_line(&mut line).await.unwrap(); // Ask
            let id = match serde_json::from_str::<GatewayMessage>(line.trim()).unwrap() {
                GatewayMessage::Ask { id, .. } => id,
                other => panic!("expected Ask, got {other:?}"),
            };
            // A decision for a *different* id must be skipped by the client.
            let wrong = HostMessage::Decision {
                id: id.wrapping_add(7),
                verdict: Verdict::Deny,
                scope: ApprovalScope::Once,
            };
            w.write_all(encode_line(&wrong).as_bytes()).await.unwrap();
            let right = HostMessage::Decision {
                id,
                verdict: Verdict::Allow,
                scope: ApprovalScope::Session,
            };
            w.write_all(encode_line(&right).as_bytes()).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let c = ControlClient::new(Some(addr), Some("tok".into()));
        assert_eq!(c.ask(&attempt(), None).await, Verdict::Allow);
    }

    #[tokio::test]
    async fn ask_fails_closed_when_host_drops_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, _w) = s.into_split();
            // Read the Hello, then drop the connection without deciding.
            let mut br = BufReader::new(r);
            let mut line = String::new();
            let _ = br.read_line(&mut line).await;
        });
        let c = ControlClient::new(Some(addr), Some("tok".into()));
        assert_eq!(c.ask(&attempt(), None).await, Verdict::Deny);
    }
}
