//! Host-side control server (TCP + per-session token).
//!
//! The gateway connects to this server and sends `ask` requests for destinations
//! whose policy is `ask`, plus `event` notifications for decisions it made itself.
//! The host routes asks to the UI and returns a verdict, and forwards events for
//! logging/activity. The host — not the agent — owns these decisions.
//!
//! Transport: **TCP**, so it works the same on Linux and inside the macOS Docker
//! VM (a bind-mounted unix socket can't cross the host↔VM file-share boundary).
//! Because a TCP port is reachable by anything that can route to it — including the
//! agent container, which shares the internal bridge with the host — the channel is
//! gated by a **per-session token**: the gateway's first line must be a matching
//! [`GatewayMessage::Hello`], or the host drops the connection. The token reaches
//! the gateway only via its container env, which the (separate) agent container
//! never sees, so the agent cannot authenticate even if it reaches the port. The
//! listener is also bound to the docker bridge IP (never `0.0.0.0`), keeping the
//! port off the LAN.

use anyhow::Result;
use cowboy_core::netproto::{
    encode_line, ApprovalScope, GatewayMessage, HostMessage, NetworkAttempt, Verdict,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

/// A pending approval the UI must decide.
pub struct ApprovalRequest {
    pub attempt: NetworkAttempt,
    /// Why the gateway is asking (e.g. "DNS tunnel suspected"), for the prompt.
    pub reason: Option<String>,
    pub reply: oneshot::Sender<(Verdict, ApprovalScope)>,
}

/// A decision the gateway reported (for the activity log / network.jsonl).
pub type NetworkEvent = (NetworkAttempt, Verdict, String);

/// Serve the control channel on a pre-bound TCP listener until cancelled. Each
/// connection must authenticate with `Hello { token }` first. Asks are sent on
/// `approvals` (each carries a reply channel); events are sent on `events`.
pub async fn serve_on(
    listener: TcpListener,
    token: String,
    approvals: mpsc::UnboundedSender<ApprovalRequest>,
    events: mpsc::UnboundedSender<NetworkEvent>,
) -> Result<()> {
    if let Ok(addr) = listener.local_addr() {
        tracing::info!(%addr, "control server listening (tcp)");
    }
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "control accept error");
                continue;
            }
        };
        // Spawn per connection: a slow/wedged gateway connection must not block
        // accepting (and thus serving approvals/events for) every other one.
        let token = token.clone();
        let approvals = approvals.clone();
        let events = events.clone();
        tokio::spawn(async move {
            let (r, w) = stream.into_split();
            if let Err(e) = handle_conn(BufReader::new(r), w, &token, &approvals, &events).await {
                tracing::debug!(%peer, error = %e, "control connection ended");
            }
        });
    }
}

/// Constant-time token comparison. The per-session token gates the control
/// channel, and the agent shares the netns and can reach the port, so a byte-wise
/// short-circuit (`==`) would be a timing oracle. Length may leak — the token is
/// a fixed-length UUID.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Handle one control connection: authenticate, then bridge asks/events. Generic
/// over the stream halves so it's transport-agnostic and unit-testable.
async fn handle_conn<R, W>(
    mut reader: BufReader<R>,
    mut w: W,
    token: &str,
    approvals: &mpsc::UnboundedSender<ApprovalRequest>,
    events: &mpsc::UnboundedSender<NetworkEvent>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut line = String::new();

    // First line MUST be a matching Hello, or we drop the connection.
    if reader.read_line(&mut line).await? == 0 {
        return Ok(());
    }
    match serde_json::from_str::<GatewayMessage>(line.trim()) {
        Ok(GatewayMessage::Hello { token: t }) if ct_eq(&t, token) => {}
        _ => {
            tracing::warn!("control connection rejected (missing/invalid token)");
            return Ok(()); // drop — no decisions for an unauthenticated peer
        }
    }

    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(()); // gateway disconnected
        }
        match serde_json::from_str::<GatewayMessage>(line.trim()) {
            Ok(GatewayMessage::Ask {
                id,
                attempt,
                reason,
            }) => {
                let (rtx, rrx) = oneshot::channel();
                let req = ApprovalRequest {
                    attempt,
                    reason,
                    reply: rtx,
                };
                if approvals.send(req).is_err() {
                    // No approver wired: fail closed.
                    write_decision(&mut w, id, Verdict::Deny, ApprovalScope::Once).await?;
                    continue;
                }
                let (verdict, scope) = rrx.await.unwrap_or((Verdict::Deny, ApprovalScope::Once));
                write_decision(&mut w, id, verdict, scope).await?;
            }
            Ok(GatewayMessage::Event {
                attempt,
                verdict,
                reason,
            }) => {
                let _ = events.send((attempt, verdict, reason));
            }
            // A second Hello (or anything else) is ignored.
            Ok(GatewayMessage::Hello { .. }) => {}
            Err(_) => { /* ignore malformed line */ }
        }
    }
}

async fn write_decision<W: AsyncWrite + Unpin>(
    w: &mut W,
    id: u64,
    verdict: Verdict,
    scope: ApprovalScope,
) -> Result<()> {
    let msg = HostMessage::Decision { id, verdict, scope };
    w.write_all(encode_line(&msg).as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::netproto::Protocol;
    use tokio::net::{TcpListener, TcpStream};

    fn attempt() -> NetworkAttempt {
        NetworkAttempt {
            protocol: Protocol::Tls,
            host: Some("example.com".into()),
            ip: None,
            port: 443,
        }
    }

    async fn spawn_server(
        token: &str,
    ) -> (
        std::net::SocketAddr,
        mpsc::UnboundedReceiver<ApprovalRequest>,
        mpsc::UnboundedReceiver<NetworkEvent>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (atx, arx) = mpsc::unbounded_channel();
        let (etx, erx) = mpsc::unbounded_channel();
        let token = token.to_string();
        tokio::spawn(async move {
            let _ = serve_on(listener, token, atx, etx).await;
        });
        (addr, arx, erx)
    }

    #[tokio::test]
    async fn authenticated_ask_gets_routed_and_decision_returned() {
        let (addr, mut arx, mut erx) = spawn_server("s3cret").await;
        let stream = TcpStream::connect(addr).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);

        // Authenticate, then ask.
        let hello = GatewayMessage::Hello {
            token: "s3cret".into(),
        };
        w.write_all(encode_line(&hello).as_bytes()).await.unwrap();
        let ask = GatewayMessage::Ask {
            id: 42,
            reason: None,
            attempt: attempt(),
        };
        w.write_all(encode_line(&ask).as_bytes()).await.unwrap();

        let req = arx.recv().await.expect("approval request");
        assert_eq!(req.attempt.host.as_deref(), Some("example.com"));
        req.reply
            .send((Verdict::Allow, ApprovalScope::Session))
            .unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let decision: HostMessage = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(
            decision,
            HostMessage::Decision {
                id: 42,
                verdict: Verdict::Allow,
                scope: ApprovalScope::Session,
            }
        );

        // Events forward too.
        let ev = GatewayMessage::Event {
            attempt: attempt(),
            verdict: Verdict::Deny,
            reason: "metadata".into(),
        };
        w.write_all(encode_line(&ev).as_bytes()).await.unwrap();
        let (a, v, reason) = erx.recv().await.expect("event");
        assert_eq!(a.port, 443);
        assert_eq!(v, Verdict::Deny);
        assert_eq!(reason, "metadata");
    }

    #[tokio::test]
    async fn wrong_token_is_dropped_without_a_decision() {
        let (addr, mut arx, _erx) = spawn_server("right").await;
        let stream = TcpStream::connect(addr).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);

        // Bad token, then an Ask — the server should drop us, never route the ask.
        let hello = GatewayMessage::Hello {
            token: "wrong".into(),
        };
        w.write_all(encode_line(&hello).as_bytes()).await.unwrap();
        let ask = GatewayMessage::Ask {
            id: 1,
            reason: None,
            attempt: attempt(),
        };
        let _ = w.write_all(encode_line(&ask).as_bytes()).await;

        // No approval request is ever routed.
        assert!(
            arx.try_recv().is_err(),
            "an unauthenticated ask must not be routed"
        );
        // The connection is closed: our read returns EOF (0 bytes).
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap();
        assert_eq!(n, 0, "server should close the unauthenticated connection");
    }
}
