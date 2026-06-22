//! `cowboy web` — a host-side server that lets a browser (e.g. a phone over
//! Tailscale) attach to a live agent session.
//!
//! It is a **thin bridge, fat client**: the browser opens a WebSocket, the
//! server connects to that session's worker unix socket and relays the
//! line-delimited `ServerMsg`/`ClientMsg` JSON both ways. All rendering happens
//! in the WASM client; the server interprets only enough to authenticate, route,
//! and validate inbound messages. A web client is just another attacher, so it
//! gets the same journal replay + multi-client guarantees as the TUI.
//!
//! Security: the server grants full control of a session, so every request
//! carries a bearer token (constant-time check, fail closed) and it binds
//! loopback by default. A non-loopback bind is refused unless it's a Tailscale
//! address (100.64.0.0/10 — Tailscale encrypts + authenticates the transport) or
//! the operator explicitly opts into an unencrypted LAN bind.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use cowboy_core::config::WebConfig;
use cowboy_core::daemonproto::{
    AttachTarget, ClientMsg, DaemonReq, DaemonResp, ServerMsg, SessionStatus, UiEventMsg,
};
use cowboy_core::netproto::encode_line;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

use crate::net::control::ct_eq;

/// Resolves a session id to its attach target. Injected so the WS bridge is
/// testable without a live daemon: production asks the daemon, tests point at a
/// fake worker socket. `None` = unknown/unreachable session.
type Resolver =
    Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, Option<AttachTarget>> + Send + Sync>;

/// Resolves a parent session id to `(root, is_live)`, so a subagent watch can
/// find the child's journal at `<root>/.cowboy/sessions/<sub>/events.jsonl` and
/// decide whether to follow it live or replay a finished one. Injected for tests.
type RootResolver = Arc<
    dyn Fn(String) -> futures::future::BoxFuture<'static, Option<(PathBuf, bool)>> + Send + Sync,
>;

struct AppState {
    token: String,
    resolve: Resolver,
    resolve_root: RootResolver,
}

// --- `cowboy web on|off|status`: manage the persistent setting ---------------

/// `cowboy web on`: enable the web UI in `web.yaml` (minting a token on first
/// use) and tell the running daemon to start serving it now.
pub async fn on(bind: Option<String>, lan: bool) -> Result<()> {
    let mut cfg = WebConfig::load_global();
    if let Some(b) = bind {
        cfg.bind = b;
    }
    if lan {
        cfg.allow_lan = true;
    }
    cfg.enabled = true;
    if cfg.token.is_empty() {
        cfg.token = uuid::Uuid::new_v4().simple().to_string();
    }
    // Validate the bind up front for immediate feedback (the daemon re-checks).
    let addr = guard_bind_str(&cfg.bind, cfg.allow_lan)?;
    cfg.save_global().context("saving web.yaml")?;

    // Apply on the running daemon (starting one if needed).
    crate::cmd::daemon::ensure_running().await?;
    let serving = matches!(
        crate::cmd::daemon::request(DaemonReq::ReloadWeb).await,
        Ok(DaemonResp::Web { serving: true })
    );
    if serving {
        println!("web UI enabled.");
    } else {
        println!("web UI enabled, but the daemon isn't serving it — check `cowboy web status`.");
    }
    print_access(&cfg, addr);
    Ok(())
}

/// `cowboy web off`: disable the web UI and stop the daemon serving it.
pub async fn off() -> Result<()> {
    let mut cfg = WebConfig::load_global();
    cfg.enabled = false;
    cfg.save_global().context("saving web.yaml")?;
    // Only poke a daemon that's already up; don't spawn one just to turn it off.
    if matches!(
        crate::cmd::daemon::request(DaemonReq::Ping).await,
        Ok(DaemonResp::Pong { .. })
    ) {
        let _ = crate::cmd::daemon::request(DaemonReq::ReloadWeb).await;
    }
    println!("web UI disabled.");
    Ok(())
}

/// `cowboy web status`: show whether it's enabled + actually serving, with the URL/QR.
pub async fn status() -> Result<()> {
    let cfg = WebConfig::load_global();
    if !cfg.enabled {
        println!("web UI: disabled (enable with `cowboy web on`)");
        return Ok(());
    }
    let serving = matches!(
        crate::cmd::daemon::request(DaemonReq::WebStatus).await,
        Ok(DaemonResp::Web { serving: true })
    );
    println!(
        "web UI: enabled · {}",
        if serving {
            "serving"
        } else {
            "not serving (is cowboyd running?)"
        }
    );
    if let Ok(addr) = guard_bind_str(&cfg.bind, cfg.allow_lan) {
        print_access(&cfg, addr);
    }
    Ok(())
}

/// Print the access URL, plus a scannable QR + warning for a remote bind.
fn print_access(cfg: &WebConfig, addr: SocketAddr) {
    let Some(url) = cfg.url() else { return };
    println!("open:  {url}");
    if !addr.ip().is_loopback() {
        if let Some(qr) = render_qr(&url) {
            println!("\n{qr}");
        }
        println!(
            "note: reachable beyond localhost — anyone with this URL can drive your sessions."
        );
    }
}

// --- the server itself (run inside cowboyd) ----------------------------------

/// Parse + validate a bind string, returning the socket address.
pub fn guard_bind_str(bind: &str, allow_lan: bool) -> Result<SocketAddr> {
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid bind address: {bind}"))?;
    guard_bind(addr.ip(), allow_lan)?;
    Ok(addr)
}

/// Serve the web UI until `cancel` fires (the daemon owns the cancel token, so
/// `cowboy web off` stops it cleanly). Resolves sessions via the daemon socket.
pub async fn serve_with(addr: SocketAddr, token: String, cancel: CancellationToken) -> Result<()> {
    let resolve: Resolver = Arc::new(|id: String| {
        Box::pin(async move {
            match crate::cmd::daemon::request(DaemonReq::AttachSession { id }).await {
                Ok(DaemonResp::Attach { target }) => Some(target),
                _ => None,
            }
        })
    });
    let resolve_root: RootResolver = Arc::new(|id: String| {
        Box::pin(async move {
            match crate::cmd::daemon::request(DaemonReq::GetSession { id }).await {
                Ok(DaemonResp::Session { info }) => {
                    Some((info.root, info.status == SessionStatus::Running))
                }
                _ => None,
            }
        })
    });
    let state = Arc::new(AppState {
        token,
        resolve,
        resolve_root,
    });
    // SO_REUSEADDR so a quick restart (daemon roll, `web off`/`on`) can rebind the
    // port while the previous listener is still in TIME_WAIT.
    let socket = if addr.is_ipv6() {
        tokio::net::TcpSocket::new_v6()
    } else {
        tokio::net::TcpSocket::new_v4()
    }
    .context("create web socket")?;
    socket.set_reuseaddr(true).ok();
    socket
        .bind(addr)
        .with_context(|| format!("binding {addr}"))?;
    let listener = socket.listen(1024).context("listen")?;
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
        .context("web server failed")?;
    Ok(())
}

/// Build the router (separated from `run` so tests can mount it on an ephemeral
/// port without the bind guard / token minting).
fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/sessions", get(list_sessions))
        .route("/api/session/{id}/ws", get(ws_handler))
        .route("/api/subagent/{parent}/{sub}/ws", get(subagent_ws_handler))
        // Static SPA assets (the trunk-built .js/.wasm). Unauthenticated — they're
        // inert code; the token still gates every /api route. /api/* and / are
        // matched first as explicit routes.
        .fallback(static_handler)
        .with_state(state)
}

/// The trunk-built WASM bundle, baked into the binary. Empty on a checkout that
/// never ran `trunk build` (then we serve [`INDEX_PLACEHOLDER`]).
#[derive(rust_embed::RustEmbed)]
#[folder = "../cowboy-web-ui/dist"] // relative to CARGO_MANIFEST_DIR; ensured by build.rs
struct WebAssets;

fn serve_asset(path: &str) -> Option<Response> {
    let file = WebAssets::get(path)?;
    let mime = match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };
    Some(([(header::CONTENT_TYPE, mime)], file.data.into_owned()).into_response())
}

/// Serve a static asset by its URL path, or 404.
async fn static_handler(uri: Uri) -> Response {
    serve_asset(uri.path().trim_start_matches('/'))
        .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

/// Refuse a bind that would leak the token in cleartext. Loopback is always
/// fine; Tailscale's CGNAT range (100.64.0.0/10) and ULA prefix are fine
/// (Tailscale provides transport encryption + device identity). Anything else
/// requires an explicit opt-in.
fn guard_bind(ip: IpAddr, allow_lan: bool) -> Result<()> {
    if ip.is_loopback() || is_tailscale(ip) || allow_lan {
        return Ok(());
    }
    bail!(
        "refusing to bind {ip}: not loopback or a Tailscale address, so the auth token would \
         travel in cleartext. Bind 127.0.0.1 and tunnel in, use your Tailscale IP \
         (100.64.0.0/10), or pass --insecure-allow-lan if this network is trusted."
    );
}

/// Render `url` as a terminal QR (half-block unicode) so a phone can scan it.
fn render_qr(url: &str) -> Option<String> {
    use qrcode::render::unicode;
    let code = qrcode::QrCode::new(url.as_bytes()).ok()?;
    Some(code.render::<unicode::Dense1x2>().quiet_zone(true).build())
}

/// Tailscale assigns IPv4 from 100.64.0.0/10 (CGNAT) and IPv6 from
/// `fd7a:115c:a1e0::/48`.
fn is_tailscale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xc0) == 0x40
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

#[derive(serde::Deserialize, Default)]
struct WsQuery {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    since_seq: Option<u64>,
}

/// True if the request presents the right token, via `Authorization: Bearer` or
/// the `?token=` query param (browsers can't set headers on a WS handshake).
fn authed(state: &AppState, headers: &HeaderMap, query_token: Option<&str>) -> bool {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or(query_token);
    presented.is_some_and(|t| ct_eq(t, &state.token))
}

async fn health() -> &'static str {
    "ok"
}

async fn index() -> Response {
    // Serve the embedded SPA shell; fall back to a placeholder if no bundle was
    // built into this binary.
    serve_asset("index.html").unwrap_or_else(|| Html(INDEX_PLACEHOLDER).into_response())
}

async fn list_sessions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<WsQuery>,
) -> Response {
    if !authed(&state, &headers, q.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match crate::cmd::daemon::request(DaemonReq::ListSessions { root: None }).await {
        Ok(DaemonResp::Sessions { sessions }) => Json(sessions).into_response(),
        _ => (StatusCode::BAD_GATEWAY, "daemon unreachable").into_response(),
    }
}

async fn ws_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<WsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !authed(&state, &headers, q.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let target = match (state.resolve)(id).await {
        Some(t) => t,
        None => return (StatusCode::NOT_FOUND, "no such session").into_response(),
    };
    match target {
        // Live: bridge to the worker socket (full control).
        AttachTarget::Live { worker_sock } => {
            let since = q.since_seq;
            ws.on_upgrade(move |socket| bridge(socket, worker_sock, since))
        }
        // Terminal: stream the on-disk journal read-only, then end (no worker).
        AttachTarget::Replay {
            journal_path,
            status,
        } => ws.on_upgrade(move |socket| replay(socket, journal_path, status)),
    }
}

/// Stream a finished session's journal to the browser as `Event`s, then `Ended`
/// — so a completed/failed session renders read-only instead of looping on a
/// dead worker socket.
async fn replay(ws: WebSocket, journal_path: PathBuf, status: SessionStatus) {
    stream_journal(
        ws,
        journal_path,
        false,
        format!("session {}", status_word(&status)),
    )
    .await;
}

/// Watch a subagent: resolve the parent's root, then stream the child's journal at
/// `<root>/.cowboy/sessions/<sub>/events.jsonl`. Follow it live while the parent
/// is running; replay once (read-only) if the parent has finished.
async fn subagent_ws_handler(
    State(state): State<Arc<AppState>>,
    Path((parent, sub)): Path<(String, String)>,
    Query(q): Query<WsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !authed(&state, &headers, q.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some((root, live)) = (state.resolve_root)(parent).await else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    // Subagent ids are opaque session ids (no slashes/`..`); reject anything that
    // could escape the sessions dir.
    if sub.is_empty() || sub.contains('/') || sub.contains("..") {
        return (StatusCode::BAD_REQUEST, "bad subagent id").into_response();
    }
    let journal = root
        .join(".cowboy")
        .join("sessions")
        .join(&sub)
        .join("events.jsonl");
    ws.on_upgrade(move |socket| stream_journal(socket, journal, live, "subagent ended".into()))
}

/// Stream a journal file to the browser as `Event`s then a final `Ended`. With
/// `follow`, tail the file as it grows (for a live subagent) until the subagent's
/// `Final` event arrives or it goes idle; without it, read once and end (replay).
///
/// Tails by byte offset (not `BufReader::lines`, which stops at EOF): each poll
/// reads from the last offset to EOF and splits off complete `\n`-terminated
/// lines, holding any trailing partial until the writer appends its newline.
async fn stream_journal(
    mut ws: WebSocket,
    journal_path: PathBuf,
    follow: bool,
    end_reason: String,
) {
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    /// Give up following a quiet journal after this long (the subagent likely died
    /// without a `Final`). Long enough not to cut off a slow-but-working subagent.
    const IDLE_GIVE_UP: Duration = Duration::from_secs(300);
    const POLL: Duration = Duration::from_millis(200);

    // The child may not have created the file yet; wait briefly when following.
    let mut file = None;
    for _ in 0..(if follow { 50 } else { 1 }) {
        if let Ok(f) = tokio::fs::File::open(&journal_path).await {
            file = Some(f);
            break;
        }
        if !follow {
            break;
        }
        tokio::time::sleep(POLL).await;
    }

    let mut seq = 0u64;
    let mut done = false;
    if let Some(mut f) = file {
        let mut pos: u64 = 0;
        let mut buf: Vec<u8> = Vec::new();
        let mut last_activity = Instant::now();
        loop {
            let mut chunk = Vec::new();
            if f.seek(std::io::SeekFrom::Start(pos)).await.is_ok() {
                if let Ok(read) = f.read_to_end(&mut chunk).await {
                    if read > 0 {
                        pos += read as u64;
                        last_activity = Instant::now();
                        buf.extend_from_slice(&chunk);
                    }
                }
            }
            // Emit every complete line currently buffered.
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                if let Ok(s) = std::str::from_utf8(&line[..line.len() - 1]) {
                    if let Ok(event) = serde_json::from_str::<UiEventMsg>(s.trim_end()) {
                        if matches!(event, UiEventMsg::Final(_)) {
                            done = true;
                        }
                        let frame = encode_line(&ServerMsg::Event { seq, event });
                        if ws
                            .send(Message::Text(frame.trim_end().to_string().into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                seq += 1;
                if done {
                    break;
                }
            }
            if done || !follow || last_activity.elapsed() > IDLE_GIVE_UP {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    let _ = ws
        .send(Message::Text(
            encode_line(&ServerMsg::Ended { reason: end_reason })
                .trim_end()
                .to_string()
                .into(),
        ))
        .await;
    let _ = ws.close().await;
}

fn status_word(s: &SessionStatus) -> &'static str {
    match s {
        SessionStatus::Completed => "completed",
        SessionStatus::Failed => "failed",
        SessionStatus::Stale => "stale",
        _ => "ended",
    }
}

/// Relay between a browser WebSocket and a session's worker unix socket: every
/// worker line becomes a WS text frame; every (well-formed) WS frame becomes a
/// `ClientMsg` line to the worker.
async fn bridge(ws: WebSocket, worker_sock: PathBuf, since_seq: Option<u64>) {
    let stream = match UnixStream::connect(&worker_sock).await {
        Ok(s) => s,
        Err(e) => {
            ended(ws, &format!("worker unreachable: {e}")).await;
            return;
        }
    };
    let (sock_r, mut sock_w) = stream.into_split();

    // Subscribe with full control + replay from the requested seq.
    let hello = encode_line(&ClientMsg::Hello {
        since_seq,
        read_only: false,
    });
    if sock_w.write_all(hello.as_bytes()).await.is_err() {
        return;
    }

    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut sock_lines = BufReader::new(sock_r).lines();

    // worker → browser
    let to_browser = async {
        while let Ok(Some(line)) = sock_lines.next_line().await {
            if ws_tx.send(Message::Text(line.into())).await.is_err() {
                break;
            }
        }
    };

    // browser → worker (validate as ClientMsg so we never inject arbitrary bytes)
    let to_worker = async {
        while let Some(Ok(msg)) = ws_rx.next().await {
            let text = match msg {
                Message::Text(t) => t.to_string(),
                Message::Close(_) => break,
                _ => continue, // ping/pong handled by axum; ignore binary
            };
            match serde_json::from_str::<ClientMsg>(&text) {
                Ok(cmd) => {
                    if sock_w
                        .write_all(encode_line(&cmd).as_bytes())
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => continue, // drop malformed frames
            }
        }
        // Browser left: detach (leave the session running) rather than End it.
        let _ = sock_w
            .write_all(encode_line(&ClientMsg::Detach).as_bytes())
            .await;
    };

    tokio::select! {
        _ = to_browser => {}
        _ = to_worker => {}
    }
}

/// Send a single `Ended` frame and close (used when we can't reach the worker).
async fn ended(mut ws: WebSocket, reason: &str) {
    let line = encode_line(&ServerMsg::Ended {
        reason: reason.to_string(),
    });
    let _ = ws
        .send(Message::Text(line.trim_end().to_string().into()))
        .await;
    let _ = ws.close().await;
}

const INDEX_PLACEHOLDER: &str = "<!doctype html><meta charset=utf-8>\
<title>cowboy web</title>\
<body style=\"font-family:system-ui;max-width:40rem;margin:3rem auto;padding:0 1rem\">\
<h1>cowboy web</h1>\
<p>The server is running. The web UI bundle isn't built into this binary yet \
(build the <code>cowboy-web-ui</code> crate with trunk).</p>\
<p>The API is live: <code>GET /api/sessions</code> and \
<code>GET /api/session/&lt;id&gt;/ws</code> (bearer token required).</p>";

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    #[test]
    fn bind_guard_allows_loopback_and_tailscale_refuses_lan() {
        // loopback
        assert!(guard_bind("127.0.0.1".parse().unwrap(), false).is_ok());
        assert!(guard_bind(IpAddr::V6(Ipv6Addr::LOCALHOST), false).is_ok());
        // tailscale v4 (100.64.0.0/10) + v6 ULA
        assert!(guard_bind("100.101.102.103".parse().unwrap(), false).is_ok());
        assert!(guard_bind("fd7a:115c:a1e0::1".parse().unwrap(), false).is_ok());
        // a non-tailscale 100.x outside the /10 is NOT tailscale (100.128.x is /9-ish)
        assert!(guard_bind("100.128.0.1".parse().unwrap(), false).is_err());
        // plain LAN refused without the opt-in, allowed with it
        assert!(guard_bind("192.168.1.50".parse().unwrap(), false).is_err());
        assert!(guard_bind("192.168.1.50".parse().unwrap(), true).is_ok());
        assert!(guard_bind("0.0.0.0".parse().unwrap(), false).is_err());
    }

    /// End-to-end relay: a browser WebSocket ↔ a fake worker unix socket. Proves
    /// the bridge sends `Hello`, forwards worker lines to the browser, and
    /// forwards (validated) browser frames back to the worker.
    #[tokio::test]
    async fn ws_bridge_relays_both_directions() {
        use cowboy_core::daemonproto::UiEventMsg;
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

        let tmp = assert_fs::TempDir::new().unwrap();
        let worker_sock = tmp.path().join("s-test.sock");

        // Fake worker: accept one client, read Hello, push an Event, then read
        // the ClientMsg the bridge forwards and report it back.
        let listener = tokio::net::UnixListener::bind(&worker_sock).unwrap();
        let (got_tx, got_rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            let hello = lines.next_line().await.unwrap().unwrap();
            assert!(hello.contains("hello"), "first line is a Hello: {hello}");
            let evt = encode_line(&ServerMsg::Event {
                seq: 0,
                event: UiEventMsg::Notice("from-worker".into()),
            });
            w.write_all(evt.as_bytes()).await.unwrap();
            // Next line is whatever the browser sent, forwarded by the bridge.
            let forwarded = lines.next_line().await.unwrap().unwrap();
            let _ = got_tx.send(forwarded);
        });

        // Mount the router with a resolver pointing at the fake worker.
        let ws = worker_sock.clone();
        let state = Arc::new(AppState {
            token: "t".into(),
            resolve: Arc::new(move |_id| {
                let ws = ws.clone();
                Box::pin(async move { Some(AttachTarget::Live { worker_sock: ws }) })
            }),
            resolve_root: Arc::new(|_| Box::pin(async { None })),
        });
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = tcp.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(tcp, router(state)).await.unwrap();
        });

        // Connect as a browser would, with the token in the query.
        let url = format!("ws://127.0.0.1:{port}/api/session/s1/ws?token=t");
        let (mut sock, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // First frame is the worker's Event, relayed verbatim.
        let frame = sock.next().await.unwrap().unwrap();
        let text = frame.into_text().unwrap();
        assert!(text.contains("from-worker"), "relayed worker event: {text}");

        // Send a ClientMsg::Message; the worker must receive it.
        let msg = serde_json::to_string(&ClientMsg::Message("hi-from-browser".into())).unwrap();
        sock.send(tokio_tungstenite::tungstenite::Message::text(msg))
            .await
            .unwrap();
        let forwarded = tokio::time::timeout(std::time::Duration::from_secs(5), got_rx)
            .await
            .expect("worker received the forwarded message")
            .unwrap();
        assert!(
            forwarded.contains("hi-from-browser"),
            "worker got the browser's message: {forwarded}"
        );
    }

    /// A WS upgrade without the token is rejected (401) — fail closed.
    #[tokio::test]
    async fn ws_without_token_is_rejected() {
        let state = Arc::new(AppState {
            token: "t".into(),
            resolve: Arc::new(|_| Box::pin(async { None })),
            resolve_root: Arc::new(|_| Box::pin(async { None })),
        });
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = tcp.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(tcp, router(state)).await.unwrap();
        });
        let url = format!("ws://127.0.0.1:{port}/api/session/s1/ws"); // no token
        assert!(
            tokio_tungstenite::connect_async(&url).await.is_err(),
            "unauthenticated WS upgrade must fail"
        );
    }

    #[test]
    fn auth_accepts_header_or_query_constant_time() {
        let state = AppState {
            token: "secret-token".into(),
            resolve: Arc::new(|_| Box::pin(async { None })),
            resolve_root: Arc::new(|_| Box::pin(async { None })),
        };
        let mut h = HeaderMap::new();
        // no creds
        assert!(!authed(&state, &h, None));
        // query param
        assert!(authed(&state, &h, Some("secret-token")));
        assert!(!authed(&state, &h, Some("wrong")));
        // bearer header
        h.insert(
            header::AUTHORIZATION,
            "Bearer secret-token".parse().unwrap(),
        );
        assert!(authed(&state, &h, None));
    }
}
