//! The macOS egress path: an authenticated proxy on the host, and nothing else.
//!
//! Seatbelt can deny traffic but cannot redirect it, so there is no transparent
//! interception to hang the policy engine off. Instead the profile allows outbound
//! connections to exactly one place — this proxy's loopback port — and commands are
//! told to use it through the usual `HTTP(S)_PROXY`/`ALL_PROXY` variables. A tool
//! that ignores them gets no network at all, and a proxy that is down means no
//! egress: containment is the profile's job, never this module's, which is the same
//! inversion the Linux transport rests on.
//!
//! Loopback is shared with the host and every other session, so the port is
//! reachable by more than this session's commands. Every request must therefore
//! carry credentials issued for **one command** of this session (`Proxy-Authorization`
//! or SOCKS5 username/password). They are revoked when the command ends, and they
//! name the command, which is how a connection is attributed without asking the
//! kernel which process owns a socket.
//!
//! The decision itself is the policy engine's, made on the host with host-resolved
//! addresses: the name the command asked for goes through the DNS policy (tunnel
//! detection, denied names), the proxy resolves it, and the connection policy is
//! evaluated for the address that will actually be dialled — so a deny CIDR such as
//! the cloud metadata address holds whatever name points at it, and there is no
//! window to rebind between the check and the dial.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use cowboy_core::netproto::{Protocol, Verdict};
use cowboy_gateway::state::GatewayState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::sandbox::exec::Ticket;

/// Largest request head accepted before the tunnel starts.
const MAX_HEAD: usize = 64 * 1024;
/// How long a client may take to say where it wants to go.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an upstream connect may take.
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
/// Unauthenticated requests tolerated on one connection. More than one, because a
/// client using `anyauth` (Apple's git) only sends credentials after a `407`.
const MAX_CHALLENGES: usize = 3;

/// Credentials issued to this session's live commands.
#[derive(Default)]
struct Creds {
    next: AtomicU64,
    /// Username → (token, the command's pid once known).
    table: Mutex<HashMap<String, (String, Option<u32>)>>,
}

impl Creds {
    /// The command pid for valid credentials; `None` for anything else.
    fn check(&self, user: &str, token: &str) -> Option<Option<u32>> {
        let table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        match table.get(user) {
            Some((t, pid)) if constant_time_eq(t.as_bytes(), token.as_bytes()) => Some(*pid),
            _ => None,
        }
    }
}

/// A running proxy for one session.
pub(crate) struct Proxy {
    port: u16,
    creds: Arc<Creds>,
    task: tokio::task::JoinHandle<()>,
}

impl Proxy {
    /// Bind an ephemeral loopback port and start serving. Must be called within the
    /// tokio runtime.
    pub fn start(engine: Arc<GatewayState>) -> Result<Self> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .context("binding the sandbox egress proxy")?;
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let port = listener.local_addr()?.port();
        let creds = Arc::new(Creds::default());
        let task = tokio::spawn(serve(listener, engine, creds.clone()));
        Ok(Self { port, creds, task })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn is_alive(&self) -> bool {
        !self.task.is_finished()
    }

    /// Credentials for one command, valid until the returned value is dropped.
    pub fn issue(&self) -> Credential {
        let user = format!("c{}", self.creds.next.fetch_add(1, Ordering::Relaxed));
        let token = uuid::Uuid::new_v4().simple().to_string();
        self.creds
            .table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(user.clone(), (token.clone(), None));
        Credential {
            user,
            token,
            port: self.port,
            creds: self.creds.clone(),
        }
    }

    /// Stop accepting, and revoke every credential so a tunnel that is still being
    /// set up fails.
    pub fn stop(&self) {
        self.task.abort();
        self.creds
            .table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One command's proxy credentials. Revoked on drop.
pub(crate) struct Credential {
    user: String,
    token: String,
    port: u16,
    creds: Arc<Creds>,
}

impl Credential {
    /// The proxy URL to hand the command.
    pub fn url(&self) -> String {
        format!(
            "http://{}:{}@127.0.0.1:{}",
            self.user, self.token, self.port
        )
    }
}

impl Ticket for Credential {
    fn spawned(&self, pid: u32) {
        if let Some(entry) = self
            .creds
            .table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&self.user)
        {
            entry.1 = Some(pid);
        }
    }
}

impl Drop for Credential {
    fn drop(&mut self) {
        self.creds
            .table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.user);
    }
}

async fn serve(listener: TcpListener, engine: Arc<GatewayState>, creds: Arc<Creds>) {
    loop {
        let conn = match listener.accept().await {
            Ok((conn, _)) => conn,
            Err(e) => {
                // Out of descriptors, most likely; retrying at once would spin.
                tracing::warn!(error = %e, "egress proxy accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let engine = engine.clone();
        let creds = creds.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(conn, &engine, &creds).await {
                tracing::debug!(error = %e, "egress proxy connection ended");
            }
        });
    }
}

async fn handle(conn: TcpStream, engine: &GatewayState, creds: &Creds) -> Result<()> {
    let mut first = [0u8; 1];
    let n = tokio::time::timeout(HEAD_TIMEOUT, conn.peek(&mut first))
        .await
        .context("client sent nothing")??;
    if n == 0 {
        return Ok(());
    }
    if first[0] == 0x05 {
        socks5(conn, engine, creds).await
    } else {
        http(conn, engine, creds).await
    }
}

/// HTTP proxying: `CONNECT host:port` tunnels, and absolute-form plain HTTP requests.
async fn http(mut conn: TcpStream, engine: &GatewayState, creds: &Creds) -> Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut challenges = 0;
    loop {
        let head_len = read_head(&mut conn, &mut buf).await?;
        let head = buf[..head_len].to_vec();
        let rest = buf[head_len..].to_vec();

        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);
        if !matches!(req.parse(&head), Ok(httparse::Status::Complete(_))) {
            return respond(&mut conn, 400, "malformed request").await;
        }
        let method = req.method.unwrap_or_default().to_string();
        let target = req.path.unwrap_or_default().to_string();
        let auth = req
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("proxy-authorization"))
            .and_then(|h| basic_credentials(h.value));

        let Some(pid) = auth.and_then(|(u, t)| creds.check(&u, &t)) else {
            challenges += 1;
            if challenges > MAX_CHALLENGES {
                return respond(&mut conn, 407, "proxy credentials required").await;
            }
            // Kept open: an `anyauth` client retries on this same connection.
            conn.write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\n\
                  Proxy-Authenticate: Basic realm=\"cowboy\"\r\n\
                  Content-Length: 0\r\n\r\n",
            )
            .await?;
            buf = rest;
            continue;
        };

        if method.eq_ignore_ascii_case("CONNECT") {
            let Some((host, port)) = split_host_port(&target, None) else {
                return respond(&mut conn, 400, "CONNECT needs host:port").await;
            };
            let protocol = if port == 443 {
                Protocol::Tls
            } else {
                Protocol::Tcp
            };
            let mut upstream = match open(engine, &host, port, protocol, pid).await {
                Ok(s) => s,
                Err(why) => return respond(&mut conn, 403, &why).await,
            };
            conn.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await?;
            if !rest.is_empty() {
                upstream.write_all(&rest).await?;
            }
            let _ = tokio::io::copy_bidirectional(&mut conn, &mut upstream).await;
            return Ok(());
        }

        // Plain HTTP in absolute form. One request per connection, so a keep-alive
        // client cannot send a second request to a host nobody decided on.
        let Some(after) = target.strip_prefix("http://") else {
            return respond(&mut conn, 400, "expected an http:// URL or CONNECT").await;
        };
        let (authority, path) = match after.find('/') {
            Some(i) => (&after[..i], &after[i..]),
            None => (after, "/"),
        };
        let Some((host, port)) = split_host_port(authority, Some(80)) else {
            return respond(&mut conn, 400, "bad host in URL").await;
        };
        let mut upstream = match open(engine, &host, port, Protocol::Http, pid).await {
            Ok(s) => s,
            Err(why) => return respond(&mut conn, 403, &why).await,
        };
        let version = if req.version == Some(0) { "1.0" } else { "1.1" };
        let mut out = format!("{method} {path} HTTP/{version}\r\n");
        for h in req.headers.iter() {
            let name = h.name.to_ascii_lowercase();
            if matches!(
                name.as_str(),
                "proxy-authorization" | "proxy-connection" | "connection" | "keep-alive"
            ) {
                continue;
            }
            out.push_str(h.name);
            out.push_str(": ");
            out.push_str(&String::from_utf8_lossy(h.value));
            out.push_str("\r\n");
        }
        out.push_str("Connection: close\r\n\r\n");
        upstream.write_all(out.as_bytes()).await?;
        if !rest.is_empty() {
            upstream.write_all(&rest).await?;
        }
        let _ = tokio::io::copy_bidirectional(&mut conn, &mut upstream).await;
        return Ok(());
    }
}

/// SOCKS5 (RFC 1928) with username/password authentication (RFC 1929), CONNECT only.
async fn socks5(mut conn: TcpStream, engine: &GatewayState, creds: &Creds) -> Result<()> {
    let mut hdr = [0u8; 2];
    conn.read_exact(&mut hdr).await?;
    let mut methods = vec![0u8; hdr[1] as usize];
    conn.read_exact(&mut methods).await?;
    if !methods.contains(&0x02) {
        // No acceptable method: credentials are not optional.
        conn.write_all(&[0x05, 0xff]).await?;
        return Ok(());
    }
    conn.write_all(&[0x05, 0x02]).await?;

    let mut ver = [0u8; 2];
    conn.read_exact(&mut ver).await?;
    let mut user = vec![0u8; ver[1] as usize];
    conn.read_exact(&mut user).await?;
    let mut plen = [0u8; 1];
    conn.read_exact(&mut plen).await?;
    let mut pass = vec![0u8; plen[0] as usize];
    conn.read_exact(&mut pass).await?;
    let pid = creds.check(
        &String::from_utf8_lossy(&user),
        &String::from_utf8_lossy(&pass),
    );
    let Some(pid) = pid else {
        conn.write_all(&[0x01, 0x01]).await?;
        return Ok(());
    };
    conn.write_all(&[0x01, 0x00]).await?;

    let mut req = [0u8; 4];
    conn.read_exact(&mut req).await?;
    if req[1] != 0x01 {
        // Only CONNECT: BIND and UDP ASSOCIATE would open inbound or datagram paths.
        conn.write_all(&socks_reply(0x07)).await?;
        return Ok(());
    }
    let host = match req[3] {
        0x01 => {
            let mut a = [0u8; 4];
            conn.read_exact(&mut a).await?;
            IpAddr::from(a).to_string()
        }
        0x04 => {
            let mut a = [0u8; 16];
            conn.read_exact(&mut a).await?;
            IpAddr::from(a).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            conn.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            conn.read_exact(&mut name).await?;
            String::from_utf8(name).context("non-UTF-8 host name")?
        }
        _ => {
            conn.write_all(&socks_reply(0x08)).await?;
            return Ok(());
        }
    };
    let mut port = [0u8; 2];
    conn.read_exact(&mut port).await?;
    let port = u16::from_be_bytes(port);

    let protocol = if port == 443 {
        Protocol::Tls
    } else {
        Protocol::Tcp
    };
    match open(engine, &host, port, protocol, pid).await {
        Ok(mut upstream) => {
            conn.write_all(&socks_reply(0x00)).await?;
            let _ = tokio::io::copy_bidirectional(&mut conn, &mut upstream).await;
        }
        Err(why) => {
            tracing::debug!(%host, port, %why, "egress refused");
            conn.write_all(&socks_reply(0x02)).await?;
        }
    }
    Ok(())
}

fn socks_reply(code: u8) -> [u8; 10] {
    [0x05, code, 0x00, 0x01, 0, 0, 0, 0, 0, 0]
}

/// Decide, then dial exactly the address that was decided on.
///
/// Returns the refusal reason as text for the client, which is the command's own
/// output: an agent told "policy: Deny" can say so, instead of guessing at a timeout.
async fn open(
    engine: &GatewayState,
    host: &str,
    port: u16,
    protocol: Protocol,
    pid: Option<u32>,
) -> std::result::Result<TcpStream, String> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let ip = match host.parse::<IpAddr>() {
        Ok(ip) => ip,
        Err(_) => {
            // The name first, before anything leaves the host: a tunnel's payload
            // *is* the query, so there is no later connection to gate.
            let v = engine.decide_dns(host, "A").await;
            if v != Verdict::Allow {
                return Err(format!("cowboy: resolving {host} is refused by policy"));
            }
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
                .await
                .map_err(|e| format!("cowboy: cannot resolve {host}: {e}"))?
                .collect();
            for a in &addrs {
                engine.record_dns(a.ip(), host.to_string());
            }
            // IPv4 first: the sandbox's own loopback is `localhost`, and a host
            // service is far more often on 127.0.0.1 than on ::1.
            addrs
                .iter()
                .map(SocketAddr::ip)
                .find(IpAddr::is_ipv4)
                .or_else(|| addrs.first().map(SocketAddr::ip))
                .ok_or_else(|| format!("cowboy: {host} has no addresses"))?
        }
    };
    let (verdict, attempt) = engine.decide_connection(ip, port, protocol).await;
    let who = pid
        .and_then(crate::sandbox::attribution::command_for)
        .unwrap_or_default();
    if verdict != Verdict::Allow {
        tracing::debug!(host = ?attempt.host, %ip, port, command = %who, ?verdict, "egress refused");
        return Err(format!(
            "cowboy: connection to {host}:{port} refused by policy ({verdict:?})"
        ));
    }
    match tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect((ip, port))).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(format!("cowboy: connecting to {host}:{port}: {e}")),
        Err(_) => Err(format!("cowboy: connecting to {host}:{port} timed out")),
    }
}

/// Read until the end of a request head, returning its length. Bytes past it stay in
/// `buf` for the caller.
async fn read_head(conn: &mut TcpStream, buf: &mut Vec<u8>) -> Result<usize> {
    let deadline = tokio::time::Instant::now() + HEAD_TIMEOUT;
    loop {
        if let Some(i) = find(buf, b"\r\n\r\n") {
            return Ok(i + 4);
        }
        if buf.len() > MAX_HEAD {
            bail!("request head too large");
        }
        let mut chunk = [0u8; 4096];
        let n = tokio::time::timeout_at(deadline, conn.read(&mut chunk))
            .await
            .context("timed out reading the request head")??;
        if n == 0 {
            bail!("client closed before finishing the request head");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn respond(conn: &mut TcpStream, code: u16, why: &str) -> Result<()> {
    let reason = match code {
        400 => "Bad Request",
        403 => "Forbidden",
        407 => "Proxy Authentication Required",
        _ => "Error",
    };
    let body = format!("{why}\n");
    let msg = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    conn.write_all(msg.as_bytes()).await?;
    let _ = conn.shutdown().await;
    Ok(())
}

/// `user:token` from a `Basic` credential.
fn basic_credentials(value: &[u8]) -> Option<(String, String)> {
    let value = std::str::from_utf8(value).ok()?.trim();
    let (scheme, b64) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (u, t) = decoded.split_once(':')?;
    Some((u.to_string(), t.to_string()))
}

/// `host:port`, `[v6]:port`, or a bare host with a default port.
fn split_host_port(s: &str, default_port: Option<u16>) -> Option<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None => default_port?,
        };
        return Some((host.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Some((h.to_string(), p.parse().ok()?)),
        _ => Some((s.to_string(), default_port?)),
    }
    .filter(|(h, _)| !h.is_empty())
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_forms() {
        assert_eq!(
            split_host_port("example.com:443", None),
            Some(("example.com".into(), 443))
        );
        assert_eq!(
            split_host_port("[::1]:8080", None),
            Some(("::1".into(), 8080))
        );
        assert_eq!(
            split_host_port("example.com", Some(80)),
            Some(("example.com".into(), 80))
        );
        assert_eq!(split_host_port("example.com", None), None);
        assert_eq!(split_host_port(":443", None), None);
    }

    #[test]
    fn basic_credentials_decode() {
        let v = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("c1:tok")
        );
        assert_eq!(
            basic_credentials(v.as_bytes()),
            Some(("c1".into(), "tok".into()))
        );
        assert_eq!(basic_credentials(b"Bearer x"), None);
    }

    #[test]
    fn a_credential_is_valid_only_until_dropped() {
        let creds = Arc::new(Creds::default());
        creds
            .table
            .lock()
            .unwrap()
            .insert("c0".into(), ("t".into(), None));
        let cred = Credential {
            user: "c0".into(),
            token: "t".into(),
            port: 1,
            creds: creds.clone(),
        };
        cred.spawned(42);
        assert_eq!(creds.check("c0", "t"), Some(Some(42)));
        assert_eq!(creds.check("c0", "wrong"), None);
        drop(cred);
        assert_eq!(creds.check("c0", "t"), None, "revoked with the command");
    }
}
