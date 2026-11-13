//! A container engine for the agent, through a filtering proxy.
//!
//! The agent never reaches the engine's own socket: a process that can talk to it
//! is you (`docker run -v $HOME:/h …`). It gets this proxy instead, a Docker Engine
//! API server on a socket in the sandbox, run **host-side** by the worker. Each
//! request is read and judged by [`filter`] before anything reaches the engine:
//! mounts must come from the project or the session scratch, privilege and host
//! namespaces are refused, every container joins an internal network with no route
//! out, pulls are limited to allowed registries, and push is refused. Every object
//! it creates is labelled with the session, which is how it tells them from your own
//! containers (the engine is shared) and how they are swept when the session ends.
//!
//! One request per connection: the proxy adds `Connection: close` and stops reading
//! the client after the request it judged, so a second request on a kept-alive
//! connection cannot ride past the filter. Docker clients reconnect as needed.
//!
//! Published ports bind the host's `127.0.0.1` and are relayed into the sandbox
//! (`NativeSandbox::publish_host_port`), so `localhost:<mapped port>` works for
//! Testcontainers and for a Compose stack's services alike.

pub mod filter;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use filter::{Route, Scope, LABEL};

/// Where the proxy's socket appears inside the sandbox, and so `DOCKER_HOST`.
pub const SANDBOX_SOCKET_DIR: &str = "/run/cowboy";
pub const SOCKET_NAME: &str = "docker.sock";

/// The largest request body the proxy will read to judge (a container definition is
/// a few KiB). Bodies it only forwards (an archive upload) are streamed, not read.
const MAX_JUDGED_BODY: usize = 4 << 20;
const MAX_HEAD: usize = 64 << 10;

/// Called with the full set of host ports this session's containers publish, after
/// anything that may have changed it.
pub type PortsFn = Arc<dyn Fn(BTreeSet<u16>) + Send + Sync>;

/// Find the engine socket: config, `$DOCKER_HOST` (unix), rootless podman's, then
/// Docker's.
pub fn engine_socket(configured: Option<&str>) -> Option<PathBuf> {
    if let Some(c) = configured {
        let p = c.strip_prefix("unix://").unwrap_or(c);
        return cowboy_core::config::expand_path(p).ok();
    }
    if let Ok(h) = std::env::var("DOCKER_HOST") {
        if let Some(p) = h.strip_prefix("unix://") {
            return Some(PathBuf::from(p));
        }
    }
    let mut candidates = Vec::new();
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(rt).join("podman/podman.sock"));
    }
    candidates.push(PathBuf::from("/var/run/docker.sock"));
    candidates.into_iter().find(|p| p.exists())
}

struct Ctx {
    scope: Scope,
    upstream: PathBuf,
    max_containers: u32,
    on_ports: PortsFn,
}

/// The running proxy. Dropping it stops serving; [`Self::shutdown`] also removes
/// everything the session created.
pub struct ContainerProxy {
    ctx: Arc<Ctx>,
    socket: PathBuf,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ContainerProxy {
    /// Listen on `socket` and serve until dropped.
    pub async fn start(
        socket: PathBuf,
        upstream: PathBuf,
        scope: Scope,
        max_containers: u32,
        on_ports: PortsFn,
    ) -> Result<Self> {
        // Check the engine answers before claiming to have one.
        let (status, _) = request(&upstream, "GET", "/_ping", None)
            .await
            .with_context(|| {
                format!(
                    "the container engine at {} did not answer",
                    upstream.display()
                )
            })?;
        if status != 200 {
            bail!("the container engine answered /_ping with {status}");
        }
        if let Some(dir) = socket.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)
            .with_context(|| format!("listening on {}", socket.display()))?;
        let ctx = Arc::new(Ctx {
            scope,
            upstream,
            max_containers,
            on_ports,
        });
        let serve = {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((conn, _)) = listener.accept().await else {
                        continue;
                    };
                    let ctx = ctx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve(conn, &ctx).await {
                            tracing::debug!(error = %e, "container proxy connection ended");
                        }
                    });
                }
            })
        };
        // Containers exit, and a port they published is then free for anything
        // else on the host to take. Keep the published set to what is running.
        let reconcile = {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    reconcile(&ctx).await;
                }
            })
        };
        Ok(Self {
            ctx,
            socket,
            tasks: vec![serve, reconcile],
        })
    }

    /// Stop serving and remove every container, network and volume this session
    /// created.
    pub async fn shutdown(&mut self) {
        for t in self.tasks.drain(..) {
            t.abort();
        }
        sweep(&self.ctx).await;
        (self.ctx.on_ports)(BTreeSet::new());
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Drop for ContainerProxy {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

fn label_filter(session: &str) -> String {
    filter::scope_query("", session)
}

async fn reconcile(ctx: &Ctx) {
    let q = label_filter(&ctx.scope.session);
    let Ok((200, body)) =
        request(&ctx.upstream, "GET", &format!("/containers/json?{q}"), None).await
    else {
        return;
    };
    let list: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut ports = BTreeSet::new();
    for c in list.as_array().into_iter().flatten() {
        for p in c
            .get("Ports")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let ip = p.get("IP").and_then(Value::as_str).unwrap_or("");
            let public = p.get("PublicPort").and_then(Value::as_u64).unwrap_or(0);
            if public > 0 && public <= u16::MAX as u64 && matches!(ip, "127.0.0.1" | "") {
                ports.insert(public as u16);
            }
        }
    }
    (ctx.on_ports)(ports);
}

async fn sweep(ctx: &Ctx) {
    let q = label_filter(&ctx.scope.session);
    let ids = |body: &[u8], key: &str| -> Vec<String> {
        let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let list = v.get("Volumes").cloned().unwrap_or(v);
        list.as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| o.get(key).and_then(Value::as_str).map(String::from))
            .collect()
    };
    if let Ok((200, b)) = request(
        &ctx.upstream,
        "GET",
        &format!("/containers/json?all=1&{q}"),
        None,
    )
    .await
    {
        for id in ids(&b, "Id") {
            let _ = request(
                &ctx.upstream,
                "DELETE",
                &format!("/containers/{id}?force=1&v=1"),
                None,
            )
            .await;
        }
    }
    if let Ok((200, b)) = request(&ctx.upstream, "GET", &format!("/networks?{q}"), None).await {
        for id in ids(&b, "Id") {
            let _ = request(&ctx.upstream, "DELETE", &format!("/networks/{id}"), None).await;
        }
    }
    if let Ok((200, b)) = request(&ctx.upstream, "GET", &format!("/volumes?{q}"), None).await {
        for name in ids(&b, "Name") {
            let _ = request(&ctx.upstream, "DELETE", &format!("/volumes/{name}"), None).await;
        }
    }
}

/// One parsed request head.
struct Head {
    method: String,
    path: String,
    query: String,
    /// Headers in order, minus the hop-by-hop ones the proxy rewrites.
    headers: Vec<(String, String)>,
    content_length: Option<usize>,
    chunked: bool,
    upgrade: bool,
}

async fn read_head<R: AsyncRead + Unpin>(r: &mut R) -> Result<(Head, Vec<u8>)> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            bail!("request head too large");
        }
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            bail!("client closed before sending a request");
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let mut headers = [httparse::EMPTY_HEADER; 96];
    let mut req = httparse::Request::new(&mut headers);
    req.parse(&buf[..end]).context("malformed request")?;
    let target = req.path.unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut head = Head {
        method: req.method.unwrap_or("").to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers: Vec::new(),
        content_length: None,
        chunked: false,
        upgrade: false,
    };
    for h in req.headers.iter() {
        let value = String::from_utf8_lossy(h.value).to_string();
        match h.name.to_ascii_lowercase().as_str() {
            "content-length" => head.content_length = value.trim().parse().ok(),
            "transfer-encoding" => head.chunked = value.to_ascii_lowercase().contains("chunked"),
            "connection" => {
                if value.to_ascii_lowercase().contains("upgrade") {
                    head.upgrade = true;
                }
            }
            "upgrade" => head.upgrade = true,
            "host" | "keep-alive" | "proxy-connection" => {}
            _ => head.headers.push((h.name.to_string(), value)),
        }
    }
    Ok((head, buf[end..].to_vec()))
}

/// Read a whole body (to judge it), decoding chunked framing.
async fn read_body<R: AsyncRead + Unpin>(
    r: &mut R,
    head: &Head,
    mut have: Vec<u8>,
) -> Result<Vec<u8>> {
    if head.chunked {
        let mut out = Vec::new();
        loop {
            let line_end = loop {
                if let Some(i) = have.windows(2).position(|w| w == b"\r\n") {
                    break i;
                }
                fill(r, &mut have).await?;
            };
            let size_line = String::from_utf8_lossy(&have[..line_end]).to_string();
            let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
                .context("bad chunk size")?;
            have.drain(..line_end + 2);
            if size == 0 {
                return Ok(out);
            }
            while have.len() < size + 2 {
                fill(r, &mut have).await?;
            }
            out.extend_from_slice(&have[..size]);
            have.drain(..size + 2);
            if out.len() > MAX_JUDGED_BODY {
                bail!("request body too large");
            }
        }
    }
    let n = head.content_length.unwrap_or(0);
    if n > MAX_JUDGED_BODY {
        bail!("request body too large");
    }
    while have.len() < n {
        fill(r, &mut have).await?;
    }
    have.truncate(n);
    Ok(have)
}

async fn fill<R: AsyncRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> Result<()> {
    let mut chunk = [0u8; 8192];
    let n = r.read(&mut chunk).await?;
    if n == 0 {
        bail!("client closed mid-body");
    }
    buf.extend_from_slice(&chunk[..n]);
    Ok(())
}

/// Forward a body the proxy does not judge, byte for byte, then stop: whatever the
/// client sends after it is not forwarded.
async fn forward_body<R, W>(r: &mut R, w: &mut W, head: &Head, mut have: Vec<u8>) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if head.chunked {
        // Re-frame as we go, so the stream ends exactly at the terminal chunk.
        loop {
            let line_end = loop {
                if let Some(i) = have.windows(2).position(|w| w == b"\r\n") {
                    break i;
                }
                fill(r, &mut have).await?;
            };
            let size_line = String::from_utf8_lossy(&have[..line_end]).to_string();
            let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
                .context("bad chunk size")?;
            w.write_all(&have[..line_end + 2]).await?;
            have.drain(..line_end + 2);
            let mut remaining = size + 2;
            while remaining > 0 {
                if have.is_empty() {
                    fill(r, &mut have).await?;
                }
                let take = remaining.min(have.len());
                w.write_all(&have[..take]).await?;
                have.drain(..take);
                remaining -= take;
            }
            if size == 0 {
                return Ok(());
            }
        }
    }
    let mut remaining = head.content_length.unwrap_or(0);
    let take = remaining.min(have.len());
    w.write_all(&have[..take]).await?;
    remaining -= take;
    let mut chunk = vec![0u8; 64 << 10];
    while remaining > 0 {
        let n = r.read(&mut chunk[..remaining.min(64 << 10)]).await?;
        if n == 0 {
            bail!("client closed mid-body");
        }
        w.write_all(&chunk[..n]).await?;
        remaining -= n;
    }
    Ok(())
}

fn request_head(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    body: BodyKind,
    upgrade: bool,
) -> Vec<u8> {
    let target = if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    };
    let mut s = format!("{method} {target} HTTP/1.1\r\nHost: docker\r\n");
    for (k, v) in headers {
        let lower = k.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "content-length" | "transfer-encoding" | "connection" | "upgrade"
        ) {
            continue;
        }
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    match body {
        BodyKind::None => {}
        BodyKind::Length(n) => s.push_str(&format!("Content-Length: {n}\r\n")),
        BodyKind::Chunked => s.push_str("Transfer-Encoding: chunked\r\n"),
    }
    if upgrade {
        s.push_str("Connection: Upgrade\r\nUpgrade: tcp\r\n");
    } else {
        s.push_str("Connection: close\r\n");
    }
    s.push_str("\r\n");
    s.into_bytes()
}

#[derive(Clone, Copy)]
enum BodyKind {
    None,
    Length(usize),
    Chunked,
}

/// A small request to the engine, for the proxy's own checks.
async fn request(
    sock: &Path,
    method: &str,
    target: &str,
    body: Option<Value>,
) -> Result<(u16, Vec<u8>)> {
    let mut up = UnixStream::connect(sock).await?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let bytes = body.map(|b| b.to_string().into_bytes());
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if bytes.is_none() {
        headers.clear();
    }
    let kind = bytes
        .as_ref()
        .map_or(BodyKind::None, |b| BodyKind::Length(b.len()));
    up.write_all(&request_head(method, path, query, &headers, kind, false))
        .await?;
    if let Some(b) = &bytes {
        up.write_all(b).await?;
    }
    let mut resp = Vec::new();
    up.read_to_end(&mut resp).await?;
    parse_response(&resp)
}

fn parse_response(resp: &[u8]) -> Result<(u16, Vec<u8>)> {
    let end = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("no response head")?
        + 4;
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut r = httparse::Response::new(&mut headers);
    r.parse(&resp[..end])?;
    let status = r.code.unwrap_or(0);
    let chunked = r.headers.iter().any(|h| {
        h.name.eq_ignore_ascii_case("transfer-encoding")
            && String::from_utf8_lossy(h.value).contains("chunked")
    });
    let mut body = resp[end..].to_vec();
    if chunked {
        let mut out = Vec::new();
        let mut rest = &body[..];
        while let Some(i) = rest.windows(2).position(|w| w == b"\r\n") {
            let size = usize::from_str_radix(
                String::from_utf8_lossy(&rest[..i])
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim(),
                16,
            )
            .unwrap_or(0);
            rest = &rest[i + 2..];
            if size == 0 || rest.len() < size {
                break;
            }
            out.extend_from_slice(&rest[..size]);
            rest = rest.get(size + 2..).unwrap_or(&[]);
        }
        body = out;
    }
    Ok((status, body))
}

async fn respond<W: AsyncWrite + Unpin>(w: &mut W, status: u16, message: &str) -> Result<()> {
    let reason = match status {
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    let body = json!({ "message": format!("cowboy: {message}") }).to_string();
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    w.write_all(head.as_bytes()).await?;
    w.write_all(body.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

/// The labels of an engine object, or `None` when it does not exist.
async fn labels_of(ctx: &Ctx, kind: &str, id: &str) -> Option<Value> {
    let path = match kind {
        "container" => format!("/containers/{}/json", enc(id)),
        "network" => format!("/networks/{}", enc(id)),
        "volume" => format!("/volumes/{}", enc(id)),
        _ => return None,
    };
    let (status, body) = request(&ctx.upstream, "GET", &path, None).await.ok()?;
    if status != 200 {
        return None;
    }
    let v: Value = serde_json::from_slice(&body).ok()?;
    let labels = match kind {
        "container" => v.get("Config").and_then(|c| c.get("Labels")).cloned(),
        _ => v.get("Labels").cloned(),
    };
    Some(labels.unwrap_or(Value::Null))
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

enum Ownership {
    Ours,
    Absent,
    Theirs,
}

async fn ownership(ctx: &Ctx, kind: &str, id: &str) -> Ownership {
    match labels_of(ctx, kind, id).await {
        None => Ownership::Absent,
        Some(l) if filter::owned(Some(&l), &ctx.scope.session) => Ownership::Ours,
        Some(_) => Ownership::Theirs,
    }
}

/// Refuse unless `id` is one of this session's `kind`s. Someone else's is reported
/// as missing, so the proxy does not confirm what else runs on the engine.
async fn require_owned<W: AsyncWrite + Unpin>(
    ctx: &Ctx,
    w: &mut W,
    kind: &str,
    id: &str,
) -> Result<bool> {
    match ownership(ctx, kind, id).await {
        Ownership::Ours => Ok(true),
        _ => {
            respond(w, 404, &format!("no such {kind} in this session: {id}")).await?;
            Ok(false)
        }
    }
}

/// The session's own network, created on first use.
async fn session_network(ctx: &Ctx) -> Result<String> {
    let short: String = ctx
        .scope
        .session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let name = format!("cowboy-{short}");
    match ownership(ctx, "network", &name).await {
        Ownership::Ours => return Ok(name),
        Ownership::Theirs => bail!("a network named {name} exists and is not this session's"),
        Ownership::Absent => {}
    }
    let (status, body) = request(
        &ctx.upstream,
        "POST",
        "/networks/create",
        Some(json!({ "Name": name, "Internal": true, "Labels": { LABEL: ctx.scope.session } })),
    )
    .await?;
    if !(200..300).contains(&status) {
        bail!(
            "creating the session network: {}",
            String::from_utf8_lossy(&body)
        );
    }
    Ok(name)
}

fn canon(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok()
}

async fn serve(mut client: UnixStream, ctx: &Ctx) -> Result<()> {
    let (head, have) = read_head(&mut client).await?;
    let route = filter::route(&head.method, &head.path, ctx.scope.build);
    tracing::debug!(method = %head.method, path = %head.path, ?route, "container proxy");

    // Judged bodies: read whole, rewrite, forward with a fresh length.
    let judged = |r: &Route| {
        matches!(
            r,
            Route::CreateContainer
                | Route::CreateExec(_)
                | Route::CreateNetwork
                | Route::CreateVolume
                | Route::NetworkConnect(_)
        )
    };
    let mut query = head.query.clone();
    let mut new_body: Option<Value> = None;
    let mut lifecycle = false;

    if judged(&route) {
        let raw = read_body(&mut client, &head, have.clone()).await?;
        let mut body: Value = if raw.iter().all(u8::is_ascii_whitespace) {
            json!({})
        } else {
            match serde_json::from_slice(&raw) {
                Ok(v) => v,
                Err(_) => return respond(&mut client, 403, "the request body is not JSON").await,
            }
        };
        let decision = judge_body(ctx, &route, &mut body).await;
        if let Err((status, msg)) = decision {
            return respond(&mut client, status, &msg).await;
        }
        lifecycle = matches!(route, Route::CreateContainer);
        new_body = Some(body);
    } else {
        match &route {
            Route::Deny(why) => return respond(&mut client, 403, why).await,
            Route::Pass => {}
            Route::Scoped => query = filter::scope_query(&query, &ctx.scope.session),
            Route::Container(id) => {
                if !require_owned(ctx, &mut client, "container", id).await? {
                    return Ok(());
                }
            }
            Route::ContainerLifecycle(id) => {
                if !require_owned(ctx, &mut client, "container", id).await? {
                    return Ok(());
                }
                lifecycle = true;
            }
            Route::Exec(id) => {
                let container = request(
                    &ctx.upstream,
                    "GET",
                    &format!("/exec/{}/json", enc(id)),
                    None,
                )
                .await
                .ok()
                .filter(|(s, _)| *s == 200)
                .and_then(|(_, b)| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| {
                    v.get("ContainerID")
                        .and_then(Value::as_str)
                        .map(String::from)
                });
                match container {
                    Some(c) if matches!(ownership(ctx, "container", &c).await, Ownership::Ours) => {
                    }
                    _ => {
                        return respond(
                            &mut client,
                            404,
                            &format!("no such exec in this session: {id}"),
                        )
                        .await
                    }
                }
            }
            Route::Network(id) => {
                if !require_owned(ctx, &mut client, "network", id).await? {
                    return Ok(());
                }
            }
            Route::Volume(id) => {
                if !require_owned(ctx, &mut client, "volume", id).await? {
                    return Ok(());
                }
            }
            Route::Pull => match filter::rewrite_pull(&query, &ctx.scope) {
                Ok(q) => query = q,
                Err(e) => return respond(&mut client, 403, &e).await,
            },
            Route::Build => query = filter::rewrite_build(&query),
            Route::Commit => {
                let container = url::form_urlencoded::parse(query.as_bytes())
                    .find(|(k, _)| k == "container")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                if !require_owned(ctx, &mut client, "container", &container).await? {
                    return Ok(());
                }
            }
            _ => unreachable!("judged routes are handled above"),
        }
    }

    let mut up = UnixStream::connect(&ctx.upstream)
        .await
        .context("connecting to the container engine")?;
    match new_body {
        Some(body) => {
            let bytes = body.to_string().into_bytes();
            let mut headers = head.headers.clone();
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-type"));
            headers.push(("Content-Type".into(), "application/json".into()));
            up.write_all(&request_head(
                &head.method,
                &head.path,
                &query,
                &headers,
                BodyKind::Length(bytes.len()),
                false,
            ))
            .await?;
            up.write_all(&bytes).await?;
        }
        None => {
            let kind = if head.chunked {
                BodyKind::Chunked
            } else if let Some(n) = head.content_length {
                BodyKind::Length(n)
            } else {
                BodyKind::None
            };
            // Only `attach` and `exec start` may become a raw stream, and only on a
            // container already confirmed as this session's. Anywhere else an
            // `Upgrade` header is dropped: it would keep the engine's connection open
            // for a second, unjudged request.
            let hijack = head.upgrade
                && matches!(route, Route::Container(_) | Route::Exec(_))
                && (head.path.ends_with("/attach") || head.path.ends_with("/start"));
            up.write_all(&request_head(
                &head.method,
                &head.path,
                &query,
                &head.headers,
                kind,
                hijack,
            ))
            .await?;
            if hijack {
                return hijacked(client, up, &head, have, ctx, lifecycle).await;
            }
            forward_body(&mut client, &mut up, &head, have).await?;
        }
    }
    // The response, whatever its length or framing, until the engine closes.
    let _ = tokio::io::copy(&mut up, &mut client).await;
    let _ = client.shutdown().await;
    if lifecycle {
        reconcile(ctx).await;
    }
    Ok(())
}

/// An `attach` / `exec start`: forward the request body, then the engine's response
/// head. Only a response that actually switched to a raw stream turns into a
/// two-way pipe; any other answer is passed back and the connection closed, so the
/// client's next bytes are never read by the engine as another request.
async fn hijacked(
    mut client: UnixStream,
    mut up: UnixStream,
    head: &Head,
    have: Vec<u8>,
    ctx: &Ctx,
    lifecycle: bool,
) -> Result<()> {
    // The body, exactly; anything the client sent after it waits in `rest`.
    let mut rest = have;
    let n = head.content_length.unwrap_or(0);
    while rest.len() < n {
        fill(&mut client, &mut rest).await?;
    }
    let stream_start = rest.split_off(n);
    up.write_all(&rest).await?;

    let mut resp = Vec::new();
    let end = loop {
        if let Some(i) = resp.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if resp.len() > MAX_HEAD {
            bail!("engine response head too large");
        }
        let mut chunk = [0u8; 4096];
        let got = up.read(&mut chunk).await?;
        if got == 0 {
            bail!("engine closed before answering");
        }
        resp.extend_from_slice(&chunk[..got]);
    };
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut r = httparse::Response::new(&mut headers);
    r.parse(&resp[..end])?;
    let status = r.code.unwrap_or(0);
    let streams = status == 101
        || (status == 200
            && r.headers.iter().any(|h| {
                h.name.eq_ignore_ascii_case("content-type")
                    && String::from_utf8_lossy(h.value).contains("vnd.docker")
            }));
    client.write_all(&resp).await?;
    if streams {
        up.write_all(&stream_start).await?;
        let _ = tokio::io::copy_bidirectional(&mut client, &mut up).await;
    } else {
        let _ = tokio::io::copy(&mut up, &mut client).await;
        let _ = client.shutdown().await;
    }
    if lifecycle {
        reconcile(ctx).await;
    }
    Ok(())
}

/// Check and rewrite a judged body. `Err((status, message))` refuses.
async fn judge_body(
    ctx: &Ctx,
    route: &Route,
    body: &mut Value,
) -> std::result::Result<(), (u16, String)> {
    let deny = |m: String| (403u16, m);
    match route {
        Route::CreateContainer => {
            let count = request(
                &ctx.upstream,
                "GET",
                &format!(
                    "/containers/json?all=1&{}",
                    label_filter(&ctx.scope.session)
                ),
                None,
            )
            .await
            .ok()
            .and_then(|(_, b)| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v.as_array().map(Vec::len))
            .unwrap_or(0);
            if count as u32 >= ctx.max_containers {
                return Err(deny(format!(
                    "this session already has {count} containers \
                     (sandbox.containers.max_containers)"
                )));
            }
            let image = body
                .get("Image")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let image_ports: Vec<String> = request(
                &ctx.upstream,
                "GET",
                &format!("/images/{}/json", enc(&image)),
                None,
            )
            .await
            .ok()
            .filter(|(s, _)| *s == 200)
            .and_then(|(_, b)| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| {
                v.get("Config")
                    .and_then(|c| c.get("ExposedPorts"))
                    .and_then(Value::as_object)
                    .map(|m| m.keys().cloned().collect())
            })
            .unwrap_or_default();
            let needs = filter::rewrite_container_create(body, &ctx.scope, &image_ports, &canon)
                .map_err(deny)?;
            if needs.default_network {
                let net = session_network(ctx)
                    .await
                    .map_err(|e| deny(e.to_string()))?;
                if let Some(hc) = body.get_mut("HostConfig").and_then(Value::as_object_mut) {
                    hc.insert("NetworkMode".into(), json!(net));
                }
                if let Some(eps) = body
                    .get_mut("NetworkingConfig")
                    .and_then(|n| n.get_mut("EndpointsConfig"))
                    .and_then(Value::as_object_mut)
                {
                    for k in ["bridge", "default"] {
                        if let Some(v) = eps.remove(k) {
                            eps.insert(net.clone(), v);
                        }
                    }
                }
            }
            for n in &needs.networks {
                if !matches!(ownership(ctx, "network", n).await, Ownership::Ours) {
                    return Err(deny(format!(
                        "network {n} is not this session's; containers may only join networks \
                         the session created"
                    )));
                }
            }
            for v in &needs.volumes {
                match ownership(ctx, "volume", v).await {
                    Ownership::Ours => {}
                    Ownership::Absent => {
                        let created = request(
                            &ctx.upstream,
                            "POST",
                            "/volumes/create",
                            Some(json!({ "Name": v, "Labels": { LABEL: ctx.scope.session } })),
                        )
                        .await;
                        if !matches!(created, Ok((s, _)) if (200..300).contains(&s)) {
                            return Err(deny(format!("could not create volume {v}")));
                        }
                    }
                    Ownership::Theirs => return Err(deny(name_clash("volume", v))),
                }
            }
            for c in &needs.containers {
                if !matches!(ownership(ctx, "container", c).await, Ownership::Ours) {
                    return Err((404, format!("no such container in this session: {c}")));
                }
            }
            Ok(())
        }
        Route::CreateExec(id) => {
            if !matches!(ownership(ctx, "container", id).await, Ownership::Ours) {
                return Err((404, format!("no such container in this session: {id}")));
            }
            filter::check_exec_create(body).map_err(deny)
        }
        Route::CreateNetwork => {
            let name = filter::rewrite_network_create(body, &ctx.scope).map_err(deny)?;
            if matches!(ownership(ctx, "network", &name).await, Ownership::Theirs) {
                return Err(deny(name_clash("network", &name)));
            }
            Ok(())
        }
        Route::CreateVolume => {
            if let Some(name) = filter::rewrite_volume_create(body, &ctx.scope).map_err(deny)? {
                if matches!(ownership(ctx, "volume", &name).await, Ownership::Theirs) {
                    return Err(deny(name_clash("volume", &name)));
                }
            }
            Ok(())
        }
        Route::NetworkConnect(net) => {
            if !matches!(ownership(ctx, "network", net).await, Ownership::Ours) {
                return Err((404, format!("no such network in this session: {net}")));
            }
            let c = body.get("Container").and_then(Value::as_str).unwrap_or("");
            if !matches!(ownership(ctx, "container", c).await, Ownership::Ours) {
                return Err((404, format!("no such container in this session: {c}")));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// A Compose project name that collides with your own stack's objects.
fn name_clash(kind: &str, name: &str) -> String {
    format!(
        "a {kind} named {name} already exists and is not this session's (likely your own \
         stack's). Use a different Compose project name, e.g. COMPOSE_PROJECT_NAME=cowboy-<x>"
    )
}
