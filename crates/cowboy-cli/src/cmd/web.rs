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
    AttachTarget, ClientMsg, DaemonReq, DaemonResp, ServerMsg, SessionInfo, SessionStatus,
    UiEventMsg,
};
use cowboy_core::netproto::encode_line;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

/// Resolves a session id to its attach target. Injected so the WS bridge is
/// testable without a live daemon: production asks the daemon, tests point at a
/// fake worker socket. `None` = unknown/unreachable session.
type Resolver =
    Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, Option<AttachTarget>> + Send + Sync>;

/// Looks a session's registry record up (`DaemonReq::GetSession`). The bridge
/// uses it to build a replay's `Snapshot`, to re-resolve a session whose worker
/// socket stopped answering, and to find (and keep checking the liveness of) a
/// subagent's parent. Injected for tests; `None` = unknown/unreachable.
type SessionLookup =
    Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, Option<SessionInfo>> + Send + Sync>;

struct AppState {
    token: String,
    resolve: Resolver,
    session: SessionLookup,
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
    let session: SessionLookup = Arc::new(|id: String| {
        Box::pin(async move {
            match crate::cmd::daemon::request(DaemonReq::GetSession { id }).await {
                Ok(DaemonResp::Session { info }) => Some(info),
                _ => None,
            }
        })
    });
    let state = Arc::new(AppState {
        token,
        resolve,
        session,
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

/// `'sha256-…'` source expressions for every inline `<script>` in the served shell.
///
/// Trunk injects its own inline module loader (the `import init …` that boots the WASM),
/// so `script-src` cannot be simply `'self'` — and `'unsafe-inline'` would throw away the
/// reason for having a CSP at all, since this page renders model-controlled markdown as
/// raw HTML. Hashing what trunk actually emitted keeps the policy strict *and* keeps it
/// correct across trunk upgrades and `--filehash` changes, neither of which we control.
///
/// Computed once from the embedded bundle. An empty list is the right answer when no
/// bundle was built (the placeholder page has no scripts).
fn inline_script_hashes() -> &'static [String] {
    static HASHES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    HASHES.get_or_init(|| {
        let Some(shell) = WebAssets::get("index.html") else {
            return Vec::new();
        };
        let html = String::from_utf8_lossy(&shell.data).into_owned();
        hash_inline_scripts(&html)
    })
}

/// The pure half of [`inline_script_hashes`], so the parsing is testable.
fn hash_inline_scripts(html: &str) -> Vec<String> {
    use base64::Engine;
    use sha2::{Digest, Sha256};

    let mut out = Vec::new();
    // Comments first: a comment explaining the policy can legitimately contain the very
    // markup being scanned for, and hashing that would produce a bogus allowance.
    let code = strip_html_comments(html);
    let mut rest = code.as_str();
    while let Some(open) = rest.find("<script") {
        let after = &rest[open + "<script".len()..];
        let Some(gt) = after.find('>') else { break };
        let (attrs, body_and_on) = (&after[..gt], &after[gt + 1..]);
        let Some(end) = body_and_on.find("</script>") else {
            break;
        };
        let body = &body_and_on[..end];
        // An external script is covered by `'self'`; only inline bodies need a hash.
        if !attrs.contains("src=") && !body.trim().is_empty() {
            let digest = Sha256::digest(body.as_bytes());
            let b64 = base64::engine::general_purpose::STANDARD.encode(digest);
            out.push(format!("'sha256-{b64}'"));
        }
        rest = &body_and_on[end + "</script>".len()..];
    }
    out
}

fn strip_html_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(open) = rest.find("<!--") {
        out.push_str(&rest[..open]);
        rest = match rest[open..].find("-->") {
            Some(close) => &rest[open + close + 3..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// The Content-Security-Policy served with every response.
///
/// `img-src` is the one that matters most: it means an image that ever slips past the
/// markdown sanitiser still cannot become an egress beacon. The transcript is injected
/// with `Html::from_html_unchecked`, so without a CSP the page's safety would rest
/// entirely on pulldown-cmark never having an escaping bug — with the bearer token behind
/// it. `connect-src` names `ws:`/`wss:` explicitly because the socket URL is built from
/// `location`, and not every browser reads `'self'` as covering the WebSocket schemes.
fn csp() -> &'static str {
    static POLICY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    POLICY.get_or_init(|| {
        let mut script = String::from("script-src 'self' 'wasm-unsafe-eval'");
        for h in inline_script_hashes() {
            script.push(' ');
            script.push_str(h);
        }
        format!(
            "default-src 'self'; \
             img-src 'self' data:; \
             style-src 'self' 'unsafe-inline'; \
             {script}; \
             connect-src 'self' ws: wss:; \
             object-src 'none'; \
             base-uri 'none'; \
             frame-ancestors 'none'"
        )
    })
}

/// Response headers applied to everything the web UI serves.
///
/// SECURITY: this page holds the bearer token in its URL query (`?token=…`, and every
/// WebSocket URL), and it renders **model-controlled markdown** as raw HTML.
/// `Referrer-Policy: no-referrer` matters because a cross-origin subresource request would
/// otherwise carry a `Referer`. Browsers default to origin-only for that, which excludes
/// the query — but a default is a browser policy, not a guarantee, and an extension or a
/// legacy enterprise setting can widen it back to the full URL. The token is not
/// something to leave resting on a default.
fn harden(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    // `insert`, not `append`: one authoritative value, so a handler cannot weaken it by
    // having set its own.
    h.insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::X_FRAME_OPTIONS,
        header::HeaderValue::from_static("DENY"),
    );
    // The token rides in the URL, so keep this page out of shared caches entirely.
    h.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    if let Ok(v) = header::HeaderValue::from_str(csp()) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    resp
}

/// Build the router (separated from `run` so tests can mount it on an ephemeral
/// port without the bind guard / token minting).
fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/sessions", get(list_sessions))
        .route("/api/session/{id}/ws", get(ws_handler))
        .route("/api/session/{id}/launchpad", get(launchpad))
        .route("/api/subagent/{parent}/{sub}/ws", get(subagent_ws_handler))
        // Static SPA assets (the trunk-built .js/.wasm). Unauthenticated — they're
        // inert code; the token still gates every /api route. /api/* and / are
        // matched first as explicit routes.
        .fallback(static_handler)
        // Applied to every route above, including the fallback and any added later —
        // a per-handler list would be one `map` away from a gap.
        .layer(axum::middleware::map_response(|r| async move { harden(r) }))
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
         (100.64.0.0/10), or pass --lan if this network is trusted."
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

/// Constant-time token comparison.
///
/// A byte-wise `==` short-circuits on the first difference, which is a timing oracle
/// for a token an attacker can guess a byte at a time. Length may leak; the token is
/// a fixed-length UUID.
///
/// Lived in the gateway control channel until that was deleted. `cowboy web` is now
/// its only consumer, so it lives here rather than in a module of one function.
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

/// The TUI's welcome openers for a session (Alt-1…9 there, buttons here), derived
/// from what the project declares. Computed host-side from the session's root, as
/// the TUI does, so both clients offer the same ones. Empty for an unknown session.
async fn launchpad(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<WsQuery>,
) -> Response {
    if !authed(&state, &headers, q.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(info) = (state.session)(id).await else {
        return Json(Vec::<String>::new()).into_response();
    };
    let root = info.root;
    let openers = tokio::task::spawn_blocking(move || {
        let paths = cowboy_core::config::ConfigPaths::for_root(&root);
        let agent = cowboy_core::config::AgentConfig::load(&paths.agent).unwrap_or_default();
        crate::cmd::session::launchpad(&root, &agent)
    })
    .await
    .unwrap_or_default();
    Json(openers).into_response()
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
    let target = match (state.resolve)(id.clone()).await {
        Some(t) => t,
        None => return (StatusCode::NOT_FOUND, "no such session").into_response(),
    };
    let since = q.since_seq;
    match target {
        // Live: bridge to the worker socket (full control).
        AttachTarget::Live { worker_sock } => {
            ws.on_upgrade(move |socket| bridge(socket, state, id, worker_sock, since))
        }
        // Terminal: stream the on-disk journal read-only, then end (no worker).
        AttachTarget::Replay {
            journal_path,
            status,
        } => {
            let info = replay_info((state.session)(id.clone()).await, &id, status);
            ws.on_upgrade(move |socket| async move {
                let (mut tx, _rx) = socket.split();
                replay(&mut tx, journal_path, info, since).await;
                let _ = tx.close().await;
            })
        }
    }
}

type WsTx = futures::stream::SplitSink<WebSocket, Message>;
type WsRx = futures::stream::SplitStream<WebSocket>;

/// The `SessionInfo` a replay's `Snapshot` carries: the daemon's record when it
/// has one, else a minimal record — either way with the replay's terminal status.
fn replay_info(info: Option<SessionInfo>, id: &str, status: SessionStatus) -> SessionInfo {
    let mut info = info.unwrap_or_else(|| SessionInfo {
        id: id.to_string(),
        root: PathBuf::new(),
        task: None,
        status,
        pid: None,
        branch: None,
        session_name: None,
        worker_sock: None,
        journal_path: None,
        lease_mode: None,
        started_ms: 0,
        last_heartbeat_ms: 0,
        turn: 0,
        tokens: (0, 0),
        attached_clients: 0,
        diffstat: String::new(),
        running_command: None,
        blocked_reason: None,
        ranch_id: None,
        workstream_id: None,
    });
    info.status = status;
    info
}

/// Send one `ServerMsg` as a text frame. `false` = the browser has gone.
async fn send_msg(tx: &mut WsTx, msg: &ServerMsg) -> bool {
    send_text(tx, encode_line(msg).trim_end().to_string()).await
}

async fn send_text(tx: &mut WsTx, text: String) -> bool {
    tx.send(Message::Text(text.into())).await.is_ok()
}

/// Stream a finished session to the browser: a `Snapshot` (the daemon's record,
/// `journal_len` = committed line count, no pending prompts — nobody is left to
/// answer them), the journal's events from `since`, then `Ended` — so a
/// completed/failed session renders read-only instead of looping on a dead
/// worker socket, and a reconnecting client resumes where it left off.
async fn replay(tx: &mut WsTx, journal_path: PathBuf, info: SessionInfo, since: Option<u64>) {
    let reason = format!("session {}", status_word(&info.status));
    // A terminal session's journal no longer grows, so one read is the whole of
    // it. A torn final record (a crash mid-append) has no newline and is not a
    // committed line, exactly as the worker's own recovery counts it.
    let mut tail = LineTail::new(journal_path);
    let lines = tail.poll().await;
    let snapshot = ServerMsg::Snapshot {
        info,
        journal_len: lines.len() as u64,
        pending_prompts: Vec::new(),
    };
    if !send_msg(tx, &snapshot).await {
        return;
    }
    let since = since.unwrap_or(0);
    for (seq, line) in (0u64..).zip(&lines) {
        if seq >= since && !send_text(tx, event_frame(seq, line)).await {
            return;
        }
    }
    let _ = send_msg(tx, &ServerMsg::Ended { reason }).await;
}

/// Tails a jsonl file by byte offset (not `BufReader::lines`, which stops at
/// EOF): each poll reads from the last offset to EOF and splits off complete
/// `\n`-terminated lines, holding any trailing partial until the writer appends
/// its newline. Opens the file lazily, so it can wait for one not yet created.
struct LineTail {
    path: PathBuf,
    file: Option<tokio::fs::File>,
    pos: u64,
    buf: Vec<u8>,
}

impl LineTail {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            pos: 0,
            buf: Vec::new(),
        }
    }

    fn exists(&self) -> bool {
        self.file.is_some()
    }

    /// Every complete line appended since the last poll (without its `\n`).
    async fn poll(&mut self) -> Vec<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        if self.file.is_none() {
            self.file = tokio::fs::File::open(&self.path).await.ok();
        }
        let Some(f) = self.file.as_mut() else {
            return Vec::new();
        };
        let mut chunk = Vec::new();
        if f.seek(std::io::SeekFrom::Start(self.pos)).await.is_ok() {
            if let Ok(read) = f.read_to_end(&mut chunk).await {
                self.pos += read as u64;
                self.buf.extend_from_slice(&chunk);
            }
        }
        let mut lines = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop();
            lines.push(line);
        }
        lines
    }
}

/// The wire frame for journal line `seq`.
///
/// The seq accounting is the worker's: seq = line index, so every line yields a
/// frame and there is never a gap the client would treat as fatal. The worker's
/// own replay (`read_journal_slice`) refuses a journal with a malformed record
/// outright — this bridge instead forwards what it can, because a finished or
/// foreign journal is exactly where a record from a newer (or older) build turns
/// up:
/// - a known event → `ServerMsg::Event`, as the worker would send it;
/// - valid JSON that isn't a known `UiEventMsg` (a newer variant) → the same
///   envelope around the raw value, for the client to skip by seq;
/// - not JSON at all → a `Notice` saying so, keeping the seq.
fn event_frame(seq: u64, line: &[u8]) -> String {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if let Ok(event) = serde_json::from_slice::<UiEventMsg>(line) {
        return encode_line(&ServerMsg::Event { seq, event })
            .trim_end()
            .to_string();
    }
    match serde_json::from_slice::<serde_json::Value>(line) {
        Ok(raw) => serde_json::json!({ "event": { "seq": seq, "event": raw } }).to_string(),
        Err(_) => encode_line(&ServerMsg::Event {
            seq,
            event: UiEventMsg::Notice("(unreadable journal line)".into()),
        })
        .trim_end()
        .to_string(),
    }
}

/// Watch a subagent: resolve the parent's root, then stream the child's journal at
/// `<root>/.cowboy/sessions/<sub>/events.jsonl` from `since_seq`.
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
    let Some(parent_info) = (state.session)(parent.clone()).await else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    // Subagent ids are opaque session ids (no slashes/`..`); reject anything that
    // could escape the sessions dir.
    if sub.is_empty() || sub.contains('/') || sub.contains("..") {
        return (StatusCode::BAD_REQUEST, "bad subagent id").into_response();
    }
    let journal = parent_info
        .root
        .join(".cowboy")
        .join("sessions")
        .join(&sub)
        .join("events.jsonl");
    let since = q.since_seq;
    ws.on_upgrade(move |socket| async move {
        let (mut tx, mut rx) = socket.split();
        watch_subagent(
            &mut tx,
            &mut rx,
            &state,
            parent,
            parent_info,
            sub,
            journal,
            since,
        )
        .await;
        let _ = tx.close().await;
    })
}

/// How often a tail re-reads its file.
const TAIL_POLL: std::time::Duration = std::time::Duration::from_millis(200);
/// How often a subagent watch re-asks the daemon whether the parent is live and
/// re-reads the parent's journal for the subagent's completion.
const PARENT_RECHECK: std::time::Duration = std::time::Duration::from_secs(1);
/// Once the watch has decided to end, keep draining until the child's journal
/// has been quiet this long — the last records can land just after the parent
/// reports the job done.
const DRAIN_QUIET: std::time::Duration = std::time::Duration::from_millis(600);

/// Stream a subagent's journal as `Event`s (from `since`), then `Ended`.
///
/// Mirrors the TUI's watch (`poll_subagent_journal`): it tails with no idle
/// timeout and no `Final` cutoff — a subagent can say `Final` and still be
/// granted more turns, and a slow model can be quiet for many minutes. What
/// bounds it instead is the **parent**: the watch ends, after draining the file,
/// once the parent session is terminal or gone, or once the parent's journal
/// reports this subagent done. A pending subagent (file not created yet) is
/// waited for on the same terms. A browser that goes away ends it immediately.
#[allow(clippy::too_many_arguments)]
async fn watch_subagent(
    tx: &mut WsTx,
    rx: &mut WsRx,
    state: &AppState,
    parent: String,
    parent_info: SessionInfo,
    sub: String,
    journal: PathBuf,
    since: Option<u64>,
) {
    use std::time::Instant;
    let since = since.unwrap_or(0);
    let mut child = LineTail::new(journal);
    let mut seq = 0u64;
    let mut parent_live = !parent_info.status.is_terminal();
    let mut parent_journal = parent_info.journal_path.map(LineTail::new);
    let mut sub_done = false;
    let mut last_check = Instant::now();
    let mut quiet_since = Instant::now();

    loop {
        let lines = child.poll().await;
        if !lines.is_empty() {
            quiet_since = Instant::now();
        }
        for line in lines {
            if seq >= since && !send_text(tx, event_frame(seq, &line)).await {
                return;
            }
            seq += 1;
        }

        // Scan the parent's journal every tick (cheap: incremental), so a done
        // report is noticed promptly; re-ask the daemon about liveness less often.
        if let Some(pj) = parent_journal.as_mut() {
            for line in pj.poll().await {
                if reports_done(&line, &sub) {
                    sub_done = true;
                }
            }
        }
        if parent_live && last_check.elapsed() >= PARENT_RECHECK {
            last_check = Instant::now();
            parent_live = (state.session)(parent.clone())
                .await
                .is_some_and(|info| !info.status.is_terminal());
        }

        let ending = !parent_live || sub_done;
        if ending && quiet_since.elapsed() >= DRAIN_QUIET {
            break;
        }
        if browser_left_within(rx, TAIL_POLL).await {
            return;
        }
    }

    let reason = if child.exists() {
        "subagent ended"
    } else {
        "subagent never started"
    };
    let _ = send_msg(
        tx,
        &ServerMsg::Ended {
            reason: reason.into(),
        },
    )
    .await;
}

/// Does this parent-journal line report subagent `sub` finished?
fn reports_done(line: &[u8], sub: &str) -> bool {
    match serde_json::from_slice::<UiEventMsg>(line) {
        Ok(UiEventMsg::SubagentDone { id, .. }) => id == sub,
        Ok(UiEventMsg::JobsChanged(jobs)) => jobs
            .iter()
            .any(|j| j.id == sub && matches!(j.state.as_str(), "done" | "failed")),
        _ => false,
    }
}

/// Wait `dur`, watching the browser side: `true` if it closed (or errored) in
/// the meantime. Anything it sends while there is nothing to relay to is dropped.
async fn browser_left_within(rx: &mut WsRx, dur: std::time::Duration) -> bool {
    let sleep = tokio::time::sleep(dur);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return false,
            msg = rx.next() => match msg {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return true,
                Some(Ok(_)) => continue,
            },
        }
    }
}

fn status_word(s: &SessionStatus) -> &'static str {
    match s {
        SessionStatus::Completed => "completed",
        SessionStatus::Failed => "failed",
        SessionStatus::Stale => "stale",
        _ => "ended",
    }
}

/// How long `bridge` keeps re-resolving a session whose registry record still
/// says live but whose worker socket refuses connections, before closing the
/// socket (without `Ended`) for the client to retry.
const UNREACHABLE_WAIT: std::time::Duration = std::time::Duration::from_secs(20);
const UNREACHABLE_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// Relay between a browser WebSocket and a session's worker unix socket.
///
/// A worker socket that refuses connections is not, by itself, the end of the
/// session: a worker that has just finished removes its socket *before* the
/// daemon records the terminal status, and a restart can move it. So instead of
/// reporting a permanent `Ended`, re-resolve through the daemon: a session that
/// is now terminal is replayed from its journal (honouring `since_seq`), and one
/// still registered live is waited on (following a changed socket path) for
/// [`UNREACHABLE_WAIT`], after which the connection closes *without* `Ended`, so
/// the client reconnects with backoff. `Ended` is sent only when the daemon has
/// nothing to show — no record, or a terminal one with no journal.
async fn bridge(
    ws: WebSocket,
    state: Arc<AppState>,
    id: String,
    worker_sock: PathBuf,
    since_seq: Option<u64>,
) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut sock = worker_sock;
    let deadline = tokio::time::Instant::now() + UNREACHABLE_WAIT;
    let stream = loop {
        let err = match crate::localsock::connect(&sock).await {
            Ok(s) => break s,
            Err(e) => e,
        };
        match (state.session)(id.clone()).await {
            None => {
                ended(&mut ws_tx, &format!("worker unreachable: {err}")).await;
                return;
            }
            Some(info) if info.status.is_terminal() => {
                match info.journal_path.clone() {
                    Some(journal) if journal.exists() => {
                        let status = info.status;
                        replay(
                            &mut ws_tx,
                            journal,
                            replay_info(Some(info), &id, status),
                            since_seq,
                        )
                        .await;
                    }
                    _ => {
                        let reason = format!("session {}", status_word(&info.status));
                        ended(&mut ws_tx, &reason).await;
                    }
                }
                let _ = ws_tx.close().await;
                return;
            }
            Some(info) => {
                if let Some(s) = info.worker_sock {
                    sock = s;
                }
                if tokio::time::Instant::now() >= deadline
                    || browser_left_within(&mut ws_rx, UNREACHABLE_POLL).await
                {
                    let _ = ws_tx.close().await;
                    return;
                }
            }
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

/// Send a single `Ended` frame and close (used when there is nothing to show).
async fn ended(tx: &mut WsTx, reason: &str) {
    let _ = send_msg(
        tx,
        &ServerMsg::Ended {
            reason: reason.to_string(),
        },
    )
    .await;
    let _ = tx.close().await;
}

const INDEX_PLACEHOLDER: &str = "<!doctype html><meta charset=utf-8>\
<title>cowboy web</title>\
<body style=\"font-family:system-ui;max-width:40rem;margin:3rem auto;padding:0 1rem\">\
<h1>cowboy web</h1>\
<p>The server is running, but this binary was built without the web UI bundle. \
Reinstall with the UI embedded: \
<code>COWBOY_WEB_UI=1 cargo install --git https://github.com/koshea/cowboy cowboy-cli</code>.</p>\
<p>The API is live: <code>GET /api/sessions</code> and \
<code>GET /api/session/&lt;id&gt;/ws</code> (bearer token required).</p>";

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    /// The bundle really did get embedded — not merely "the build compiled".
    ///
    /// The embed happens in a derive macro reading `../cowboy-web-ui/dist` at compile
    /// time, and an absent or stale bundle degrades *silently* to a placeholder page:
    /// the binary builds, `cowboy web` starts, and the browser gets an apology. CI
    /// built the bundle before the binary and called that end-to-end coverage, but
    /// nothing checked the result.
    ///
    /// Self-skips on an ordinary dev checkout, where an empty `dist/` is the expected
    /// state. `COWBOY_WEB_UI_TESTS=required` turns the skip into a failure, which is
    /// what CI and the release build set — otherwise this test would pass by doing
    /// nothing precisely when it matters.
    #[test]
    fn the_web_ui_bundle_is_embedded_when_it_was_built() {
        let files: Vec<String> = WebAssets::iter().map(|f| f.to_string()).collect();
        let required = std::env::var("COWBOY_WEB_UI_TESTS").as_deref() == Ok("required");
        if files.is_empty() {
            assert!(
                !required,
                "COWBOY_WEB_UI_TESTS=required but no web UI bundle is embedded — run \
                 `trunk build --release` in crates/cowboy-web-ui before building"
            );
            eprintln!("skipping: no bundle embedded (empty dist/ — run `trunk build`)");
            return;
        }
        assert!(
            files.iter().any(|f| f == "index.html"),
            "a bundle is embedded but has no index.html, so the SPA shell would still \
             fall back to the placeholder: {files:?}"
        );
        assert!(
            files.iter().any(|f| f.ends_with("_bg.wasm")),
            "no wasm in the bundle — the page would load and do nothing: {files:?}"
        );
        assert!(
            serve_asset("index.html").is_some(),
            "index.html is embedded but not servable"
        );
    }

    /// Every response carries the security headers, including ones added later.
    ///
    /// Served over a real socket rather than by calling `harden` directly: the value of
    /// putting it in a `layer` is that a route added tomorrow is covered without anyone
    /// remembering to, and only routing a real request proves that.
    #[tokio::test]
    async fn every_route_is_hardened() {
        let state = Arc::new(AppState {
            token: "t".into(),
            resolve: Arc::new(|_| Box::pin(async { None })),
            session: Arc::new(|_| Box::pin(async { None })),
        });
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = tcp.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(tcp, router(state)).await.unwrap();
        });

        let client = reqwest::Client::new();
        // The SPA shell, an unauthenticated API route, an authenticated one that will
        // 401, and the static fallback — the four shapes a response can take.
        for path in ["/", "/api/health", "/api/sessions", "/does-not-exist.js"] {
            let resp = client
                .get(format!("http://127.0.0.1:{port}{path}"))
                .send()
                .await
                .expect("request");
            let h = resp.headers();
            assert_eq!(
                h.get(header::REFERRER_POLICY).map(|v| v.to_str().unwrap()),
                Some("no-referrer"),
                "{path} must not leak a referer: the token is in the URL"
            );
            let csp = h
                .get(header::CONTENT_SECURITY_POLICY)
                .map(|v| v.to_str().unwrap())
                .unwrap_or_default();
            // The clauses with security meaning, rather than the whole string, so tuning
            // the policy does not require re-approving a literal.
            assert!(
                csp.contains("img-src 'self' data:"),
                "{path}: the CSP must stop a model-emitted image reaching the network: {csp}"
            );
            assert!(
                csp.contains("script-src 'self' 'wasm-unsafe-eval'"),
                "{path}: script-src missing: {csp}"
            );
            assert!(
                !csp.contains("'unsafe-inline'")
                    || !csp
                        .split("script-src")
                        .nth(1)
                        .unwrap_or("")
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .contains("'unsafe-inline'"),
                "{path}: script-src must stay strict — 'unsafe-inline' defeats it: {csp}"
            );
            assert_eq!(
                h.get(header::X_CONTENT_TYPE_OPTIONS)
                    .map(|v| v.to_str().unwrap()),
                Some("nosniff"),
                "{path}"
            );
            assert_eq!(
                h.get(header::CACHE_CONTROL).map(|v| v.to_str().unwrap()),
                Some("no-store"),
                "{path} holds a token in its URL and must not be cached"
            );
        }
    }

    /// The page's only script is a file, not inline.
    ///
    /// This is what lets the CSP keep `script-src 'self'`. If the helper moves back
    /// inline the CSP silently stops it from running — a broken viewport on mobile, with
    /// nothing failing — so pin the shape rather than trusting the comment in the HTML.
    /// The CSP allows exactly the inline scripts the served bundle contains.
    ///
    /// This is the test that matters, and the one I initially got wrong: I checked the
    /// *source* `index.html`, which has no inline script — but trunk rewrites the shell
    /// and injects its own inline module loader, so a `script-src 'self'` policy would
    /// have blocked the WASM from ever booting. The served file is the only one with a
    /// vote. Self-skips without a bundle, like the embed test next to it.
    #[test]
    fn the_csp_allows_the_bundles_own_loader_without_unsafe_inline() {
        let Some(shell) = WebAssets::get("index.html") else {
            eprintln!("skipping: no bundle embedded (empty dist/ — run `trunk build`)");
            return;
        };
        let html = String::from_utf8_lossy(&shell.data).into_owned();
        let hashes = hash_inline_scripts(&html);
        let policy = csp();

        // Whatever inline scripts the shell has, each must be individually allowed.
        assert!(
            !hashes.is_empty(),
            "trunk has always injected an inline loader; if that changed, this test should \
             be relaxed deliberately rather than silently passing"
        );
        for h in &hashes {
            assert!(policy.contains(h.as_str()), "{h} missing from: {policy}");
        }
        // And the escape hatch stays shut — with it, the hashes would be ignored and the
        // whole policy would stop mitigating an escaping bug in the markdown renderer.
        let script_src = policy
            .split("script-src")
            .nth(1)
            .and_then(|s| s.split(';').next())
            .unwrap_or_default();
        assert!(
            !script_src.contains("'unsafe-inline'"),
            "script-src must stay strict: {script_src}"
        );
    }

    #[test]
    fn inline_script_hashing_ignores_comments_and_external_scripts() {
        // A comment explaining the policy legitimately contains the markup it warns
        // about; hashing that would mint an allowance for text no browser executes.
        let hashes = hash_inline_scripts(
            r#"<!-- an inline <script>evil()</script> would need unsafe-inline -->
               <script src="viewport.js"></script>
               <script type="module">boot()</script>"#,
        );
        assert_eq!(hashes.len(), 1, "only the real inline body: {hashes:?}");
        // Pinned against a known value so a change in digest or encoding is visible.
        assert_eq!(
            hashes[0],
            format!("'sha256-{}'", {
                use base64::Engine;
                use sha2::{Digest, Sha256};
                base64::engine::general_purpose::STANDARD.encode(Sha256::digest(b"boot()"))
            })
        );
    }

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
            session: Arc::new(|_| Box::pin(async { None })),
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
            session: Arc::new(|_| Box::pin(async { None })),
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

    /// Mount the router on an ephemeral port; returns the port.
    async fn serve_test(state: AppState) -> u16 {
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = tcp.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(tcp, router(Arc::new(state))).await.unwrap();
        });
        port
    }

    /// Read every frame (as raw JSON, since an unknown event is not a
    /// `ServerMsg`) until the server closes, with an overall timeout.
    async fn frames(url: &str) -> Vec<serde_json::Value> {
        let (mut sock, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let mut out = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while let Some(Ok(frame)) = sock.next().await {
                if let Ok(text) = frame.into_text() {
                    if !text.is_empty() {
                        out.push(serde_json::from_str(&text).unwrap());
                    }
                }
            }
        })
        .await
        .expect("server closed the socket");
        out
    }

    fn line(event: &UiEventMsg) -> String {
        serde_json::to_string(event).unwrap() + "\n"
    }

    fn seqs(frames: &[serde_json::Value]) -> Vec<u64> {
        frames
            .iter()
            .filter_map(|f| f["event"]["seq"].as_u64())
            .collect()
    }

    #[test]
    fn journal_lines_always_yield_a_frame_at_their_seq() {
        let known: serde_json::Value =
            serde_json::from_str(&event_frame(3, br#"{"notice":"hi"}"#)).unwrap();
        assert_eq!(
            known,
            serde_json::json!({"event":{"seq":3,"event":{"notice":"hi"}}})
        );
        // A newer build's variant: forwarded raw, for the client to skip by seq.
        let unknown: serde_json::Value =
            serde_json::from_str(&event_frame(4, br#"{"from_the_future":{"x":1}}"#)).unwrap();
        assert_eq!(
            unknown,
            serde_json::json!({"event":{"seq":4,"event":{"from_the_future":{"x":1}}}})
        );
        // Not JSON: a notice, still at its seq.
        let garbage: ServerMsg = serde_json::from_str(&event_frame(5, b"not json{")).unwrap();
        assert_eq!(
            garbage,
            ServerMsg::Event {
                seq: 5,
                event: UiEventMsg::Notice("(unreadable journal line)".into())
            }
        );
    }

    /// A finished session: Snapshot (daemon record, terminal status, full
    /// journal_len), then events from `since_seq` with no seq gaps even across
    /// unparseable lines, then Ended.
    #[tokio::test]
    async fn replay_sends_snapshot_and_resumes_at_since_seq() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let journal = tmp.path().join("events.jsonl");
        let body = [
            line(&UiEventMsg::Notice("zero".into())),
            line(&UiEventMsg::Notice("one".into())),
            "{\"from_the_future\":1}\n".to_string(),
            "garbage\n".to_string(),
            line(&UiEventMsg::Final("four".into())),
            "{\"torn".to_string(), // no newline: not committed
        ]
        .concat();
        std::fs::write(&journal, body).unwrap();

        let jp = journal.clone();
        let port = serve_test(AppState {
            token: "t".into(),
            resolve: Arc::new(move |_| {
                let journal_path = jp.clone();
                Box::pin(async move {
                    Some(AttachTarget::Replay {
                        journal_path,
                        status: SessionStatus::Completed,
                    })
                })
            }),
            session: Arc::new(|id| {
                Box::pin(async move {
                    let mut info = replay_info(None, &id, SessionStatus::Running);
                    info.task = Some("the task".into());
                    info.tokens = (7, 8);
                    Some(info)
                })
            }),
        })
        .await;

        let got = frames(&format!(
            "ws://127.0.0.1:{port}/api/session/s1/ws?token=t&since_seq=1"
        ))
        .await;
        let snap: ServerMsg = serde_json::from_value(got[0].clone()).unwrap();
        match snap {
            ServerMsg::Snapshot {
                info,
                journal_len,
                pending_prompts,
            } => {
                assert_eq!(journal_len, 5);
                assert_eq!(info.status, SessionStatus::Completed);
                assert_eq!(info.task.as_deref(), Some("the task"));
                assert_eq!(info.tokens, (7, 8));
                assert!(pending_prompts.is_empty());
            }
            other => panic!("expected Snapshot first, got {other:?}"),
        }
        assert_eq!(seqs(&got), vec![1, 2, 3, 4], "from since_seq, no gaps");
        assert_eq!(
            got[2]["event"]["event"],
            serde_json::json!({"from_the_future":1})
        );
        assert!(got.last().unwrap().get("ended").is_some(), "{got:?}");
    }

    /// A worker socket that is gone is re-resolved: the session is now terminal,
    /// so the browser gets the replay (from since_seq), not "worker unreachable".
    #[tokio::test]
    async fn bridge_falls_back_to_replay_when_the_worker_is_gone() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let journal = tmp.path().join("events.jsonl");
        std::fs::write(
            &journal,
            [
                line(&UiEventMsg::Notice("a".into())),
                line(&UiEventMsg::Notice("b".into())),
            ]
            .concat(),
        )
        .unwrap();
        let dead_sock = tmp.path().join("gone.sock");
        let jp = journal.clone();
        let port = serve_test(AppState {
            token: "t".into(),
            resolve: Arc::new(move |_| {
                let worker_sock = dead_sock.clone();
                Box::pin(async move { Some(AttachTarget::Live { worker_sock }) })
            }),
            session: Arc::new(move |id| {
                let jp = jp.clone();
                Box::pin(async move {
                    let mut info = replay_info(None, &id, SessionStatus::Failed);
                    info.journal_path = Some(jp);
                    Some(info)
                })
            }),
        })
        .await;

        let got = frames(&format!(
            "ws://127.0.0.1:{port}/api/session/s1/ws?token=t&since_seq=1"
        ))
        .await;
        assert!(got[0].get("snapshot").is_some(), "{got:?}");
        assert_eq!(seqs(&got), vec![1]);
        let end: ServerMsg = serde_json::from_value(got.last().unwrap().clone()).unwrap();
        assert_eq!(
            end,
            ServerMsg::Ended {
                reason: "session failed".into()
            }
        );
    }

    /// A subagent watch waits for a not-yet-created journal, keeps tailing past
    /// `Final`, honours since_seq, and ends only once the parent is terminal and
    /// the file is drained.
    #[tokio::test]
    async fn subagent_watch_follows_past_final_until_the_parent_ends() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let tmp = assert_fs::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let dir = root.join(".cowboy/sessions/sub1");
        let parent_live = Arc::new(AtomicBool::new(true));
        let live = parent_live.clone();
        let r = root.clone();
        let port = serve_test(AppState {
            token: "t".into(),
            resolve: Arc::new(|_| Box::pin(async { None })),
            session: Arc::new(move |id| {
                let status = if live.load(Ordering::SeqCst) {
                    SessionStatus::Running
                } else {
                    SessionStatus::Completed
                };
                let mut info = replay_info(None, &id, status);
                info.root = r.clone();
                Box::pin(async move { Some(info) })
            }),
        })
        .await;

        let url = format!("ws://127.0.0.1:{port}/api/subagent/p1/sub1/ws?token=t&since_seq=1");
        let reader = tokio::spawn(async move { frames(&url).await });

        // Pending: the journal appears a while after the watch starts.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        std::fs::write(
            &path,
            [
                line(&UiEventMsg::Notice("0".into())),
                line(&UiEventMsg::Final("first answer".into())),
            ]
            .concat(),
        )
        .unwrap();
        // More work after Final (a granted extra turn).
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(line(&UiEventMsg::Notice("2".into())).as_bytes())
                .unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            !reader.is_finished(),
            "still following while the parent is live"
        );
        parent_live.store(false, Ordering::SeqCst);

        let got = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
            .await
            .expect("watch ends once the parent is terminal")
            .unwrap();
        assert_eq!(seqs(&got), vec![1, 2], "{got:?}");
        assert!(got.last().unwrap().get("ended").is_some(), "{got:?}");
    }

    #[test]
    fn auth_accepts_header_or_query_constant_time() {
        let state = AppState {
            token: "secret-token".into(),
            resolve: Arc::new(|_| Box::pin(async { None })),
            session: Arc::new(|_| Box::pin(async { None })),
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
