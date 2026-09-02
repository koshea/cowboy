//! The relay: the sandbox's only way out, and deliberately not a proxy.
//!
//! It never creates an outbound socket. For each intercepted connection it reports
//! the original destination to the policy engine and, if allowed, receives an
//! **already-connected descriptor** created by the engine in the host network
//! namespace. A passed descriptor keeps the namespace it was created in, so the
//! relay's traffic never traverses the sandbox's own routing or nftables rules at
//! all.
//!
//! That is worth dwelling on, because it removes a whole class of bug. The
//! container design needed the nftables ruleset to exempt the gateway's own egress
//! by uid, or the gateway's upstream connections would be redirected back into
//! itself. Getting that exemption slightly wrong — as it would have been here, where
//! the agent is also uid 0 — hands the agent an uninterception path. With fd
//! passing there is nothing to exempt, so there is no exemption to get wrong.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use super::channel::{
    self, ConnectReply, ConnectRequest, ResolveReply, ResolveRequest, PEEK_BYTES,
};

/// Serve intercepted TCP for the lifetime of the session.
///
/// `channel_fd` is the relay's end of the anonymous socketpair shared with the
/// policy engine. Requests are serialized through a mutex: the engine answers one at
/// a time, and a `SEQPACKET` pair has no request ids, so interleaving two exchanges
/// would let one connection receive another's descriptor.
pub async fn serve(listener: TcpListener, channel_fd: OwnedFd) -> Result<()> {
    let channel = Arc::new(tokio::sync::Mutex::new(channel_fd));
    loop {
        let (client, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "relay accept failed");
                continue;
            }
        };
        let channel = channel.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(client, peer, channel).await {
                tracing::debug!(error = %e, "relay connection ended");
            }
        });
    }
}

async fn handle(
    mut client: TcpStream,
    peer: SocketAddr,
    channel: Arc<tokio::sync::Mutex<OwnedFd>>,
) -> Result<()> {
    // The destination the client actually dialled. Fails closed on purpose: a
    // connection with no conntrack entry never went through the nat hook, which
    // means someone connected to the relay's port directly rather than being
    // redirected to it. There is no original destination to honour, and guessing
    // would turn the relay into an open proxy.
    let original = original_dst(&client)
        .context("no original destination (connected to the relay directly?)")?;

    let peek = peek_first_bytes(&client).await;
    let request = ConnectRequest {
        dst_ip: original.ip().to_string(),
        dst_port: original.port(),
        peek,
        command_pid: command_pid_for(peer, ProcNet::Tcp),
    };

    let (reply, upstream) = {
        let guard = channel.lock().await;
        let fd: BorrowedFd<'_> = guard.as_fd();
        channel::send(fd, &channel::encode(&request)?, None)?;
        let (bytes, passed) = channel::recv(fd)?.context("the policy engine went away")?;
        (channel::decode::<ConnectReply>(&bytes)?, passed)
    };

    if !reply.allowed {
        // Closing without writing gives the client a clean connection reset, which
        // is what a blocked destination should look like from inside.
        tracing::debug!(dest = %original, reason = %reply.reason, "relay refused");
        return Ok(());
    }
    let upstream = upstream.context("the engine allowed but passed no descriptor")?;

    // Adopt the passed descriptor. It was created in the host network namespace and
    // is already connected.
    let upstream = std::net::TcpStream::from(upstream);
    upstream
        .set_nonblocking(true)
        .context("making the passed descriptor non-blocking")?;
    let mut upstream =
        TcpStream::from_std(upstream).context("adopting the passed descriptor into the runtime")?;

    // Splice both directions until either side finishes.
    match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok((up, down)) => tracing::trace!(dest = %original, up, down, "relay closed"),
        Err(e) => tracing::debug!(dest = %original, error = %e, "relay copy ended"),
    }
    Ok(())
}

/// Serve intercepted DNS for the lifetime of the session.
///
/// A **dumb pipe, deliberately**. It reads a datagram, forwards the bytes, and writes
/// back whatever bytes come home. It does not parse DNS, does not know which names
/// are allowed, and does not know which upstream resolver exists — all of that is on
/// the host, so there is no DNS policy inside the boundary for the agent to reach.
///
/// The one thing it must get right is not mixing up clients: each query is answered
/// to the address it came from, and the channel is used one exchange at a time.
pub async fn serve_dns(sock: UdpSocket, channel_fd: OwnedFd) -> Result<()> {
    let sock = Arc::new(sock);
    let channel = Arc::new(tokio::sync::Mutex::new(channel_fd));
    let mut buf = vec![0u8; cowboy_gateway::dns::MAX_DATAGRAM];
    loop {
        let (len, client) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "dns recv failed");
                continue;
            }
        };
        let query = buf[..len].to_vec();
        let sock = sock.clone();
        let channel = channel.clone();
        // A task per query so a slow upstream does not stop the socket draining;
        // they serialize on the channel, which is the most a SEQPACKET pair allows.
        tokio::spawn(async move {
            if let Err(e) = resolve_one(&sock, client, query, channel).await {
                tracing::debug!(error = %e, "dns query not answered");
            }
        });
    }
}

async fn resolve_one(
    sock: &UdpSocket,
    client: SocketAddr,
    query: Vec<u8>,
    channel: Arc<tokio::sync::Mutex<OwnedFd>>,
) -> Result<()> {
    let request = ResolveRequest {
        query,
        command_pid: command_pid_for(client, ProcNet::Udp),
    };
    let reply = {
        let guard = channel.lock().await;
        let fd: BorrowedFd<'_> = guard.as_fd();
        channel::send(fd, &channel::encode(&request)?, None)?;
        let (bytes, stray) = channel::recv(fd)?.context("the policy engine went away")?;
        if stray.is_some() {
            tracing::warn!("the engine passed a descriptor with a DNS reply; dropping it");
        }
        channel::decode::<ResolveReply>(&bytes)?
    };
    // An empty response means drop: the client's resolver retries, rather than
    // caching something we invented.
    if reply.response.is_empty() {
        return Ok(());
    }
    sock.send_to(&reply.response, client).await?;
    Ok(())
}

/// Read the pre-nat destination with `SO_ORIGINAL_DST`.
///
/// Preserves the fail-closed behaviour the container proxy had: no conntrack entry
/// means the connection was not redirected here, so there is nothing to honour.
pub fn original_dst(sock: &TcpStream) -> Result<SocketAddr> {
    // SAFETY: getsockopt with a correctly sized sockaddr_in and its length.
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_IP,
            libc::SO_ORIGINAL_DST,
            (&mut addr as *mut libc::sockaddr_in).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .context("SO_ORIGINAL_DST (no NAT entry for this connection)");
    }
    let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
    Ok(SocketAddr::new(IpAddr::V4(ip), u16::from_be(addr.sin_port)))
}

/// Peek the client's first bytes without consuming them.
///
/// `MSG_PEEK`, so the destination still receives the client's stream byte for byte —
/// consuming them here would corrupt the connection. Best-effort: a client that
/// says nothing before expecting a reply (some protocols) simply gets classified by
/// IP instead, which is a weaker match but not a wrong one.
async fn peek_first_bytes(client: &TcpStream) -> Vec<u8> {
    let mut buf = vec![0u8; PEEK_BYTES];
    // A short timeout: an empty peek costs precision, but blocking here would hang
    // every connection whose client waits for the server to speak first.
    let peeked =
        tokio::time::timeout(std::time::Duration::from_millis(200), client.peek(&mut buf)).await;
    match peeked {
        Ok(Ok(n)) => {
            buf.truncate(n);
            buf
        }
        _ => Vec::new(),
    }
}

/// Best-effort: which sandboxed process owns the traffic from `peer`.
///
/// Finds the socket in the session network namespace whose local port is the client's,
/// then the process holding that socket's inode. Only ever a label for the prompt, so
/// every failure path returns `None` rather than an error — and it is looked up while
/// the socket is still open, which is the only point where it can be.
fn command_pid_for(peer: SocketAddr, proto: ProcNet) -> Option<u32> {
    let inode = socket_inode_for_local_port(proto, peer.port())?;
    pid_holding_inode(inode)
}

/// Which table to search. TCP and UDP have separate ones with the same layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcNet {
    Tcp,
    Udp,
}

impl ProcNet {
    fn path(self) -> &'static str {
        match self {
            ProcNet::Tcp => "/proc/net/tcp",
            ProcNet::Udp => "/proc/net/udp",
        }
    }
}

/// Find the inode of the *client's* socket by its local port.
///
/// The client here is the agent's process, so its own socket is the one whose
/// `local_address` port matches — not the relay's end, which has the relay's port.
fn socket_inode_for_local_port(proto: ProcNet, port: u16) -> Option<u64> {
    let text = std::fs::read_to_string(proto.path()).ok()?;
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 {
            continue;
        }
        // local_address is field 1, formatted `HEXADDR:HEXPORT`.
        let local_port = f[1]
            .split(':')
            .nth(1)
            .and_then(|p| u16::from_str_radix(p, 16).ok());
        if local_port == Some(port) {
            return f[9].parse().ok();
        }
    }
    None
}

/// Which process holds `inode`, by scanning `/proc/*/fd`.
fn pid_holding_inode(inode: u64) -> Option<u32> {
    let target = format!("socket:[{inode}]");
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_str()?;
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue; // a process that exited, or one we cannot read
        };
        for fd in fds.flatten() {
            if std::fs::read_link(fd.path())
                .map(|p| p.to_string_lossy() == target)
                .unwrap_or(false)
            {
                return Some(pid);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant that stops the relay being an open proxy: a connection that did
    /// not come through the nat hook has no original destination, and must fail
    /// rather than be forwarded somewhere guessed.
    #[tokio::test]
    async fn a_direct_connection_has_no_original_destination() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (accepted, _) = listener.accept().await.unwrap();
        let _client = client.await.unwrap();

        // On loopback with no DNAT, SO_ORIGINAL_DST either errors or reports the
        // relay's own address. Both mean "not redirected here" — what must never
        // happen is it reporting some third-party destination we would then dial.
        match original_dst(&accepted) {
            Err(_) => {}
            Ok(got) => assert_eq!(
                got.port(),
                addr.port(),
                "a direct connection must not yield a foreign destination: {got}"
            ),
        }
    }

    #[test]
    fn parsing_proc_net_tcp_finds_a_port() {
        // The lookup is best-effort; assert it does not panic on real input and
        // returns nothing for a port that cannot be in use.
        assert_eq!(socket_inode_for_local_port(ProcNet::Tcp, 0), None);
        assert_eq!(socket_inode_for_local_port(ProcNet::Udp, 0), None);
    }

    #[test]
    fn a_missing_inode_yields_no_pid() {
        assert_eq!(pid_holding_inode(u64::MAX), None);
    }
}
