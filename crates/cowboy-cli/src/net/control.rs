//! Host-side control socket server.
//!
//! The gateway connects to this unix socket and sends `ask` requests for
//! destinations whose policy is `ask`, plus `event` notifications for decisions
//! it made itself. The host routes asks to the UI and returns a verdict, and
//! forwards events for logging/activity. The host — not the agent — owns these
//! decisions.

use std::path::PathBuf;

use anyhow::{Context, Result};
use cowboy_core::netproto::{
    encode_line, ApprovalScope, GatewayMessage, HostMessage, NetworkAttempt, Verdict,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

/// A pending approval the UI must decide.
pub struct ApprovalRequest {
    pub attempt: NetworkAttempt,
    pub reply: oneshot::Sender<(Verdict, ApprovalScope)>,
}

/// A decision the gateway reported (for the activity log / network.jsonl).
pub type NetworkEvent = (NetworkAttempt, Verdict, String);

/// Serve the control socket until cancelled. Asks are sent on `approvals`
/// (each carries a reply channel); events are sent on `events`.
pub async fn serve(
    path: PathBuf,
    approvals: mpsc::UnboundedSender<ApprovalRequest>,
    events: mpsc::UnboundedSender<NetworkEvent>,
) -> Result<()> {
    let _ = std::fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding control socket {}", path.display()))?;
    tracing::info!(sock = %path.display(), "control socket listening");

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "control accept error");
                continue;
            }
        };
        let (r, w) = stream.into_split();
        if let Err(e) = handle_conn(r, w, &approvals, &events).await {
            tracing::debug!(error = %e, "control connection ended");
        }
    }
}

async fn handle_conn(
    r: tokio::net::unix::OwnedReadHalf,
    mut w: tokio::net::unix::OwnedWriteHalf,
    approvals: &mpsc::UnboundedSender<ApprovalRequest>,
    events: &mpsc::UnboundedSender<NetworkEvent>,
) -> Result<()> {
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(()); // gateway disconnected
        }
        match serde_json::from_str::<GatewayMessage>(line.trim()) {
            Ok(GatewayMessage::Ask { id, attempt }) => {
                let (rtx, rrx) = oneshot::channel();
                let req = ApprovalRequest {
                    attempt,
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
            Err(_) => { /* ignore malformed line */ }
        }
    }
}

async fn write_decision(
    w: &mut tokio::net::unix::OwnedWriteHalf,
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
    use tokio::net::UnixStream;

    fn attempt() -> NetworkAttempt {
        NetworkAttempt {
            protocol: Protocol::Tls,
            host: Some("example.com".into()),
            ip: None,
            port: 443,
        }
    }

    #[tokio::test]
    async fn ask_gets_routed_and_decision_returned() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("ctrl.sock");
        let (atx, mut arx) = mpsc::unbounded_channel();
        let (etx, mut erx) = mpsc::unbounded_channel();
        let serve_sock = sock.clone();
        tokio::spawn(async move {
            let _ = serve(serve_sock, atx, etx).await;
        });

        // Wait for the socket, then connect as the "gateway".
        let mut stream = None;
        for _ in 0..50 {
            if let Ok(s) = UnixStream::connect(&sock).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let stream = stream.expect("connect to control socket");
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);

        // Send an Ask; an approval request should arrive, we approve it.
        let ask = GatewayMessage::Ask {
            id: 42,
            attempt: attempt(),
        };
        w.write_all(encode_line(&ask).as_bytes()).await.unwrap();

        let req = arx.recv().await.expect("approval request");
        assert_eq!(req.attempt.host.as_deref(), Some("example.com"));
        req.reply
            .send((Verdict::Allow, ApprovalScope::Session))
            .unwrap();

        // The decision is written back for our id.
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

        // An event is forwarded.
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
}
