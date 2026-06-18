//! The proxy listeners: explicit CONNECT, transparent TLS (SNI), and
//! transparent HTTP (Host). Each recovers the intended destination, asks the
//! [`GatewayState`] for a verdict, and on allow splices bytes to the upstream
//! with `tokio::io::copy_bidirectional`. Nothing is decrypted (no MITM).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use cowboy_core::netproto::{NetworkAttempt, Protocol, Verdict};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::{PORT_CONNECT, PORT_TLS};
use crate::http::{parse_connect, parse_host_header};
use crate::sni::{extract_sni, SniResult};
use crate::state::GatewayState;

/// Spawn the two TCP listeners. Returns when any listener fails to bind.
///
/// All of the agent's TCP is REDIRECTed to the transparent listener (any port);
/// the CONNECT listener serves proxy-aware clients that dial it explicitly.
pub async fn run(state: Arc<GatewayState>) -> Result<()> {
    let connect = TcpListener::bind(("0.0.0.0", PORT_CONNECT))
        .await
        .context("bind CONNECT proxy")?;
    let transparent = TcpListener::bind(("0.0.0.0", PORT_TLS))
        .await
        .context("bind transparent proxy listener")?;
    tracing::info!(
        connect = PORT_CONNECT,
        transparent = PORT_TLS,
        "proxy listeners up"
    );

    let s1 = state.clone();
    let s2 = state;
    tokio::try_join!(
        accept_loop(connect, s1, Mode::Connect),
        accept_loop(transparent, s2, Mode::Transparent),
    )?;
    Ok(())
}

#[derive(Clone, Copy)]
enum Mode {
    Connect,
    Transparent,
}

async fn accept_loop(listener: TcpListener, state: Arc<GatewayState>, mode: Mode) -> Result<()> {
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "accept error");
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, state, mode).await {
                tracing::debug!(error = %e, "connection handler ended");
            }
        });
    }
}

async fn handle(client: TcpStream, state: Arc<GatewayState>, mode: Mode) -> Result<()> {
    match mode {
        Mode::Connect => handle_connect(client, state).await,
        Mode::Transparent => {
            let orig = original_dst(&client)?;
            handle_transparent(client, state, orig).await
        }
    }
}

/// Explicit-proxy path: read the CONNECT line, decide, then tunnel.
async fn handle_connect(mut client: TcpStream, state: Arc<GatewayState>) -> Result<()> {
    let mut buf = Vec::with_capacity(256);
    let mut tmp = [0u8; 256];
    let target = loop {
        let n = client.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        match parse_connect(&buf) {
            Ok(Some(t)) => break t,
            Ok(None) => continue,
            Err(()) => {
                let _ = client.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
                return Ok(());
            }
        }
    };

    let attempt = NetworkAttempt {
        protocol: Protocol::Tls,
        host: Some(target.host.clone()),
        ip: None,
        port: target.port,
    };
    if state.decide(&attempt).await != Verdict::Allow {
        let _ = client.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
        return Ok(());
    }

    let mut upstream = TcpStream::connect((target.host.as_str(), target.port))
        .await
        .context("dialing CONNECT target")?;
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Bounded peek: read into `buf` until a parser classifies the stream (TLS via
/// SNI, then HTTP via Host) or it's clearly neither. Returns the recovered host,
/// if any. Wrapped in a timeout by the caller so server-speaks-first/opaque
/// protocols (which send nothing first) don't block — reads are cancellation-safe,
/// so any unconsumed bytes stay in the socket for the splice.
async fn sniff_host(client: &mut TcpStream, buf: &mut Vec<u8>) -> Option<String> {
    let mut tmp = [0u8; 1024];
    loop {
        match client.read(&mut tmp).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
        match extract_sni(buf) {
            SniResult::Found(h) => return Some(h),
            SniResult::NoSni => return None, // TLS without SNI → IP fallback
            SniResult::NotTls => match parse_host_header(buf) {
                Ok(Some(h)) => return Some(h),
                Ok(None) if buf.len() > 16384 => return None,
                Ok(None) => {} // plausibly HTTP, need more bytes
                Err(()) => return None, // not HTTP either → opaque
            },
            SniResult::Incomplete if buf.len() > 16384 => return None,
            SniResult::Incomplete => {} // plausibly TLS, need more bytes
        }
    }
}

/// Unified transparent path (any port). Sniffs the first bytes to recover a host
/// from TLS SNI or the HTTP Host header; for opaque or server-speaks-first
/// protocols it attributes by the connection's IP via the DNS map. Then decides
/// and splices, replaying the buffered bytes so the upstream sees an intact stream.
async fn handle_transparent(
    mut client: TcpStream,
    state: Arc<GatewayState>,
    orig: SocketAddr,
) -> Result<()> {
    let mut buf = Vec::with_capacity(1024);
    // HTTP/TLS clients speak first immediately; if nothing classifiable arrives
    // quickly, treat it as opaque and attribute by IP (no hang).
    let host = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        sniff_host(&mut client, &mut buf),
    )
    .await
    .unwrap_or(None);

    let attempt = state.enrich(NetworkAttempt {
        protocol: if host.is_some() {
            Protocol::Tls
        } else {
            Protocol::Tcp
        },
        host,
        ip: Some(orig.ip()),
        port: orig.port(),
    });
    if state.decide(&attempt).await != Verdict::Allow {
        return Ok(()); // drop: closing the socket fails the connection
    }

    let mut upstream = TcpStream::connect(orig)
        .await
        .context("dialing upstream")?;
    if !buf.is_empty() {
        upstream.write_all(&buf).await?; // replay buffered client bytes
    }
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Recover the pre-REDIRECT destination via `SO_ORIGINAL_DST`. The socket option
/// is Linux-only; the gateway only ever runs inside its Linux container, so on
/// other targets (e.g. a macOS host workspace build) this compiles down to the
/// local-address fallback and is never exercised at runtime.
fn original_dst(stream: &TcpStream) -> Result<SocketAddr> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let sock = socket2::SockRef::from(stream);
        if let Ok(addr) = sock.original_dst() {
            if let Some(a) = addr.as_socket() {
                return Ok(a);
            }
        }
    }
    // Fall back to the local address (best effort).
    Ok(stream.local_addr()?)
}
