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

    // The host we authorized is the host we dial (the proxy resolves it itself), so
    // a spoofed name can't reach a different address on this path.
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
/// so any unconsumed bytes stay in the socket for the splice. Generic over the
/// reader so it's unit-testable without a live socket.
async fn sniff_host<R: tokio::io::AsyncRead + Unpin>(
    client: &mut R,
    buf: &mut Vec<u8>,
) -> Option<String> {
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
                Ok(None) => {}          // plausibly HTTP, need more bytes
                Err(()) => return None, // not HTTP either → opaque
            },
            SniResult::Incomplete if buf.len() > 16384 => return None,
            SniResult::Incomplete => {} // plausibly TLS, need more bytes
        }
    }
}

/// Unified transparent path (any port). Authorizes the connection by the
/// hostname(s) **this gateway resolved** for the dialed IP (its policy resolver
/// records them), then splices, replaying the buffered bytes so the upstream sees
/// an intact stream.
///
/// SECURITY: the client picks its own SNI/Host and we never MITM, so a sniffed
/// name must NOT drive the verdict — otherwise an agent reaches any IP by sending
/// an allow-listed SNI. We still sniff, but only to classify the protocol and to
/// flag a name that wasn't among what we resolved for the IP (a spoof attempt).
/// (Residual: a host co-located on an allow-listed CDN IP is reachable — inherent
/// to IP-based filtering without MITM.)
async fn handle_transparent(
    mut client: TcpStream,
    state: Arc<GatewayState>,
    orig: SocketAddr,
) -> Result<()> {
    let mut buf = Vec::with_capacity(1024);
    // HTTP/TLS clients speak first immediately; if nothing classifiable arrives
    // quickly, treat it as opaque (no hang). The sniffed name is telemetry only.
    let sniffed = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        sniff_host(&mut client, &mut buf),
    )
    .await
    .unwrap_or(None);
    let protocol = if sniffed.is_some() {
        Protocol::Tls
    } else {
        Protocol::Tcp
    };

    let (verdict, _attempt) = state
        .decide_connection(orig.ip(), orig.port(), protocol)
        .await;

    // Spoof signal: the client presented a name we never resolved for this IP.
    if let Some(claimed) = &sniffed {
        let resolved = state.dns().lookup_all(orig.ip());
        if !resolved.iter().any(|r| host_eq(r, claimed)) {
            tracing::warn!(
                sni = %claimed, ip = %orig.ip(), ?verdict,
                "client-presented name was not resolved for this IP (authorized by resolved name / IP)"
            );
        }
    }

    if verdict != Verdict::Allow {
        return Ok(()); // drop: closing the socket fails the connection
    }

    let mut upstream = TcpStream::connect(orig).await.context("dialing upstream")?;
    if !buf.is_empty() {
        upstream.write_all(&buf).await?; // replay buffered client bytes
    }
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Case-insensitive hostname equality ignoring a trailing dot (for spoof logging).
fn host_eq(a: &str, b: &str) -> bool {
    a.trim_end_matches('.')
        .eq_ignore_ascii_case(b.trim_end_matches('.'))
}

/// Recover the pre-REDIRECT destination via `SO_ORIGINAL_DST`. Fails closed: if
/// the option can't be read (no REDIRECT in the path) we error rather than fall
/// back to `local_addr()`, which under REDIRECT is the proxy's own address and
/// would mis-attribute the connection. The gateway only runs inside its Linux
/// container; the non-Linux arm exists solely so a host workspace build compiles.
fn original_dst(stream: &TcpStream) -> Result<SocketAddr> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let sock = socket2::SockRef::from(stream);
        let addr = sock
            .original_dst()
            .context("SO_ORIGINAL_DST failed (no REDIRECT in path); failing closed")?;
        addr.as_socket()
            .context("SO_ORIGINAL_DST returned a non-IP address")
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        Ok(stream.local_addr()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `sniff_host` over a fixed byte sequence (mock reader yields it, then EOF).
    async fn sniff(bytes: &[u8]) -> Option<String> {
        let mut reader = tokio_test::io::Builder::new().read(bytes).build();
        let mut buf = Vec::new();
        sniff_host(&mut reader, &mut buf).await
    }

    #[tokio::test]
    async fn sniffs_http_host() {
        let req = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        assert_eq!(sniff(req).await.as_deref(), Some("example.com"));
    }

    #[tokio::test]
    async fn sniffs_tls_sni() {
        let bytes = crate::sni::tls_record(&crate::sni::client_hello_with_sni("api.github.com"));
        assert_eq!(sniff(&bytes).await.as_deref(), Some("api.github.com"));
    }

    #[tokio::test]
    async fn opaque_traffic_has_no_host() {
        // Not TLS (no 0x16 prefix) and not HTTP → no host, so the connection is
        // attributed by IP (the security-critical fallback).
        assert_eq!(sniff(&[0x00, 0x01, 0x02, 0x03, 0xff, 0xfe]).await, None);
    }

    #[test]
    fn host_eq_ignores_case_and_trailing_dot() {
        assert!(host_eq("GitHub.com.", "github.com"));
        assert!(!host_eq("evil.com", "github.com"));
    }
}
