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

use crate::config::{PORT_CONNECT, PORT_HTTP, PORT_TLS};
use crate::http::{parse_connect, parse_host_header};
use crate::sni::{extract_sni, SniResult};
use crate::state::GatewayState;

/// Spawn all three TCP listeners. Returns when any listener fails to bind.
pub async fn run(state: Arc<GatewayState>) -> Result<()> {
    let connect = TcpListener::bind(("0.0.0.0", PORT_CONNECT))
        .await
        .context("bind CONNECT proxy")?;
    let tls = TcpListener::bind(("0.0.0.0", PORT_TLS))
        .await
        .context("bind transparent TLS listener")?;
    let http = TcpListener::bind(("0.0.0.0", PORT_HTTP))
        .await
        .context("bind transparent HTTP listener")?;
    tracing::info!(
        connect = PORT_CONNECT,
        tls = PORT_TLS,
        http = PORT_HTTP,
        "proxy listeners up"
    );

    let s1 = state.clone();
    let s2 = state.clone();
    let s3 = state;
    tokio::try_join!(
        accept_loop(connect, s1, Mode::Connect),
        accept_loop(tls, s2, Mode::TransparentTls),
        accept_loop(http, s3, Mode::TransparentHttp),
    )?;
    Ok(())
}

#[derive(Clone, Copy)]
enum Mode {
    Connect,
    TransparentTls,
    TransparentHttp,
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

async fn handle(mut client: TcpStream, state: Arc<GatewayState>, mode: Mode) -> Result<()> {
    match mode {
        Mode::Connect => handle_connect(client, state).await,
        Mode::TransparentTls => {
            let orig = original_dst(&client)?;
            handle_transparent_tls(client, state, orig).await
        }
        Mode::TransparentHttp => {
            let orig = original_dst(&client)?;
            handle_transparent_http(&mut client, state, orig).await
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

/// Transparent TLS path: peek the SNI, decide, then splice (prepending the
/// buffered ClientHello bytes so the handshake reaches the upstream intact).
async fn handle_transparent_tls(
    mut client: TcpStream,
    state: Arc<GatewayState>,
    orig: SocketAddr,
) -> Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    let host = loop {
        let n = client.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        match extract_sni(&buf) {
            SniResult::Found(h) => break Some(h),
            SniResult::NoSni | SniResult::NotTls => break None,
            SniResult::Incomplete if buf.len() > 16384 => break None,
            SniResult::Incomplete => continue,
        }
    };

    let attempt = state.enrich(NetworkAttempt {
        protocol: Protocol::Tls,
        host,
        ip: Some(orig.ip()),
        port: orig.port(),
    });
    if state.decide(&attempt).await != Verdict::Allow {
        return Ok(()); // drop: closing the socket fails the TLS handshake
    }

    let mut upstream = TcpStream::connect(orig)
        .await
        .context("dialing TLS upstream")?;
    upstream.write_all(&buf).await?; // replay buffered ClientHello
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Transparent plaintext path: read the Host header, decide, then splice.
async fn handle_transparent_http(
    client: &mut TcpStream,
    state: Arc<GatewayState>,
    orig: SocketAddr,
) -> Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    let host = loop {
        let n = client.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        match parse_host_header(&buf) {
            Ok(Some(h)) => break Some(h),
            Ok(None) if buf.len() > 16384 => break None,
            Ok(None) => continue,
            Err(()) => break None,
        }
    };

    let attempt = state.enrich(NetworkAttempt {
        protocol: Protocol::Http,
        host,
        ip: Some(orig.ip()),
        port: orig.port(),
    });
    if state.decide(&attempt).await != Verdict::Allow {
        let _ = client.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
        return Ok(());
    }

    let mut upstream = TcpStream::connect(orig)
        .await
        .context("dialing HTTP upstream")?;
    upstream.write_all(&buf).await?;
    tokio::io::copy_bidirectional(client, &mut upstream).await?;
    Ok(())
}

/// Recover the pre-REDIRECT destination via `SO_ORIGINAL_DST`.
fn original_dst(stream: &TcpStream) -> Result<SocketAddr> {
    let sock = socket2::SockRef::from(stream);
    if let Ok(addr) = sock.original_dst() {
        if let Some(a) = addr.as_socket() {
            return Ok(a);
        }
    }
    // Fall back to the local address (best effort).
    Ok(stream.local_addr()?)
}
