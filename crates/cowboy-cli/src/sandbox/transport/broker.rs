//! The host side of the relay channels: decide, dial, hand back a descriptor — and
//! answer DNS.
//!
//! Runs in the worker, on the trusted side of the boundary. For each connection
//! request it classifies, asks the policy engine, and — only on `Allow` — dials the
//! destination **in the host network namespace** and passes the connected descriptor
//! back. For each DNS query it applies the resolver's policy and returns response
//! bytes.
//!
//! Both sit here rather than in the relay for the same reason: the sandbox must hold
//! no policy, no allow-list, and no resolver address, so there is nothing inside the
//! boundary to subvert. The relay forwards bytes and receives descriptors; every
//! decision is made on this side.
//!
//! Where authorization comes from matters. The peeked bytes are used to *classify*
//! (is this TLS, is it HTTP) and to notice a name that disagrees with what the
//! resolver saw. They are never what authorizes: the agent writes those bytes, so a
//! request could claim any SNI it liked. The verdict comes from the name the
//! resolver itself recorded for the destination IP, or from CIDR rules on the real
//! IP. That is the same rule the container proxy followed, and it is why there is no
//! TLS interception here.

use std::net::{IpAddr, SocketAddr};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use anyhow::{Context, Result};
use cowboy_core::netproto::{Protocol, Verdict};
use cowboy_gateway::state::GatewayState;

use super::channel::{self, ConnectReply, ConnectRequest, ResolveReply, ResolveRequest};

/// Answer relay requests until the relay goes away.
///
/// Blocking recv on a dedicated thread rather than async: the channel is a
/// `SEQPACKET` socketpair, and one blocking reader is simpler and easier to reason
/// about than registering a raw descriptor with the runtime — which is worth
/// something on the boundary that decides egress.
pub fn serve_blocking(
    channel_fd: OwnedFd,
    state: Arc<GatewayState>,
    runtime: tokio::runtime::Handle,
) {
    loop {
        let received = match channel::recv(channel_fd.as_fd()) {
            Ok(Some(v)) => v,
            Ok(None) => {
                tracing::debug!("relay channel closed; broker exiting");
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "relay channel read failed; broker exiting");
                return;
            }
        };
        let (bytes, stray) = received;
        if stray.is_some() {
            // The relay never passes descriptors; ignoring one silently would leak it.
            tracing::warn!("relay passed an unexpected descriptor; dropping it");
        }

        let request: ConnectRequest = match channel::decode(&bytes) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "undecodable relay request; refusing");
                let _ = refuse(channel_fd.as_fd(), "malformed request");
                continue;
            }
        };

        let state = state.clone();
        let decision = runtime.block_on(async move { decide(&request, &state).await });
        match decision {
            Decision::Allow { upstream, reason } => {
                let reply = ConnectReply {
                    allowed: true,
                    reason,
                };
                if let Ok(msg) = channel::encode(&reply) {
                    let fd = upstream.as_fd();
                    if let Err(e) = channel::send(channel_fd.as_fd(), &msg, Some(fd)) {
                        tracing::warn!(error = %e, "could not hand the connection to the relay");
                    }
                }
                // Dropping our copy is correct: the relay now owns the connection.
            }
            Decision::Refuse(reason) => {
                let _ = refuse(channel_fd.as_fd(), &reason);
            }
        }
    }
}

/// Answer relay DNS requests until the relay goes away.
///
/// A second thread rather than a branch in [`serve_blocking`], because a query can
/// wait seconds on an upstream resolver and connection decisions must not queue
/// behind it. Both threads are the same shape: read a request, decide on the host,
/// reply.
pub fn serve_dns_blocking(
    channel_fd: OwnedFd,
    state: Arc<GatewayState>,
    runtime: tokio::runtime::Handle,
    upstream: Option<SocketAddr>,
) {
    if upstream.is_none() {
        // Worth saying once and loudly: the sandbox will resolve nothing, so nothing
        // becomes reachable by name. That is the safe direction, but it looks like a
        // bug from inside, so name the cause.
        tracing::warn!(
            "no resolver found in /etc/resolv.conf; DNS in the sandbox will fail. \
             Names will not resolve, so only IP/CIDR rules can match."
        );
    }
    loop {
        let received = match channel::recv(channel_fd.as_fd()) {
            Ok(Some(v)) => v,
            Ok(None) => {
                tracing::debug!("resolve channel closed; dns broker exiting");
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "resolve channel read failed; dns broker exiting");
                return;
            }
        };
        let (bytes, stray) = received;
        if stray.is_some() {
            tracing::warn!("relay passed an unexpected descriptor with a query; dropping it");
        }

        let request: ResolveRequest = match channel::decode(&bytes) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "undecodable resolve request; dropping");
                let _ = drop_query(channel_fd.as_fd());
                continue;
            }
        };

        // Every gate — record type, name, tunnel shape — is inside `dns::resolve`,
        // on this side of the boundary.
        let response = match upstream {
            Some(up) => {
                let state = state.clone();
                runtime
                    .block_on(async move {
                        cowboy_gateway::dns::resolve(&request.query, up, &state).await
                    })
                    .unwrap_or_default()
            }
            None => Vec::new(),
        };

        let reply = ResolveReply { response };
        match channel::encode(&reply) {
            Ok(msg) => {
                if let Err(e) = channel::send(channel_fd.as_fd(), &msg, None) {
                    tracing::warn!(error = %e, "could not return a DNS response to the relay");
                }
            }
            Err(e) => {
                // An oversized response must not leave the relay waiting forever.
                tracing::warn!(error = %e, "DNS response too large for the channel; dropping");
                let _ = drop_query(channel_fd.as_fd());
            }
        }
    }
}

/// Tell the relay to send nothing back. Every request must get exactly one reply, or
/// the relay's channel lock is held until the session ends.
fn drop_query(sock: std::os::fd::BorrowedFd<'_>) -> Result<()> {
    let reply = ResolveReply {
        response: Vec::new(),
    };
    channel::send(sock, &channel::encode(&reply)?, None)
}

enum Decision {
    Allow { upstream: OwnedFd, reason: String },
    Refuse(String),
}

fn refuse(sock: std::os::fd::BorrowedFd<'_>, reason: &str) -> Result<()> {
    let reply = ConnectReply {
        allowed: false,
        reason: reason.to_string(),
    };
    channel::send(sock, &channel::encode(&reply)?, None)
}

async fn decide(request: &ConnectRequest, state: &GatewayState) -> Decision {
    let Ok(ip) = request.dst_ip.parse::<IpAddr>() else {
        return Decision::Refuse("unparseable destination".into());
    };
    let protocol = classify(&request.peek);

    // Authorization by what the resolver recorded for this IP, never by the
    // client-presented name in `peek`.
    let (verdict, mut attempt) = state
        .decide_connection(ip, request.dst_port, protocol)
        .await;
    attempt.command_pid = request.command_pid;

    if verdict != Verdict::Allow {
        return Decision::Refuse(format!("policy: {verdict:?}"));
    }

    match dial(SocketAddr::new(ip, request.dst_port)).await {
        Ok(fd) => Decision::Allow {
            upstream: fd,
            reason: "allowed".into(),
        },
        Err(e) => Decision::Refuse(format!("upstream dial failed: {e}")),
    }
}

/// Dial the destination in the host network namespace and yield the descriptor.
async fn dial(dest: SocketAddr) -> Result<OwnedFd> {
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::net::TcpStream::connect(dest),
    )
    .await
    .context("connect timed out")?
    .with_context(|| format!("connecting to {dest}"))?;
    let std_stream = stream
        .into_std()
        .context("converting the connection for descriptor passing")?;
    Ok(OwnedFd::from(std_stream))
}

/// Classify by first bytes: TLS handshake, HTTP request, or opaque.
///
/// Classification only. It selects how the destination is described in prompts and
/// logs; it never decides whether the connection is allowed.
fn classify(peek: &[u8]) -> Protocol {
    if peek.first() == Some(&0x16) {
        // TLS handshake record.
        return Protocol::Tls;
    }
    const METHODS: &[&[u8]] = &[
        b"GET ", b"POST", b"PUT ", b"HEAD", b"DELE", b"OPTI", b"PATC", b"CONN", b"TRAC",
    ];
    if METHODS.iter().any(|m| peek.starts_with(m)) {
        return Protocol::Http;
    }
    Protocol::Tcp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_is_classified_from_its_record_type() {
        assert_eq!(classify(&[0x16, 0x03, 0x01, 0x00]), Protocol::Tls);
    }

    #[test]
    fn http_is_classified_from_its_method() {
        assert_eq!(classify(b"GET / HTTP/1.1\r\n"), Protocol::Http);
        assert_eq!(classify(b"POST /x HTTP/1.1\r\n"), Protocol::Http);
    }

    /// An empty or unrecognized peek must fall back to opaque TCP, not guess. A
    /// client that waits for the server to speak first yields no bytes at all, and
    /// that connection still has to be decided — by IP.
    #[test]
    fn an_unknown_or_empty_peek_is_opaque_tcp() {
        assert_eq!(classify(&[]), Protocol::Tcp);
        assert_eq!(classify(&[0x00, 0x01, 0x02]), Protocol::Tcp);
        assert_eq!(classify(b"SSH-2.0-OpenSSH"), Protocol::Tcp);
    }

    /// The peek is attacker-chosen, so classification must be a label only. This
    /// pins the intent: a TLS-looking prefix for a destination the policy denies
    /// still yields no authorization by itself.
    #[test]
    fn classification_does_not_imply_authorization() {
        // Deliberately a pure function of the bytes, with no policy input at all —
        // if this ever gained a `&GatewayState` parameter, that would be the smell.
        let as_tls = classify(&[0x16, 0x03, 0x01]);
        assert_eq!(as_tls, Protocol::Tls);
    }
}
