//! `cowboyd` — the local coordination daemon (control plane).
//!
//! Listens on a per-user Unix socket and maintains a persistent registry of
//! sessions and worktree leases. It does NOT host agent loops or sit in the
//! event-stream data path; sessions run as separate worker processes.
//!
//! This module also exposes the client-side helpers (`socket_path`, `request`,
//! `ensure_running`) that the `cowboy` CLI uses to reach the daemon.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use cowboy_core::daemonproto::{
    DaemonReq, DaemonRequest, DaemonResp, DaemonResponse, LeaseMode, SessionId, SessionInfo,
};
use cowboy_core::netproto::encode_line;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Paths (client and daemon must agree)
// ---------------------------------------------------------------------------

/// Per-user runtime dir for sockets/lock (`$XDG_RUNTIME_DIR/cowboy`, else
/// `/tmp/cowboy-$UID`).
pub fn runtime_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(d).join("cowboy");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/cowboy-{uid}"))
}

/// The daemon control socket path.
pub fn socket_path() -> PathBuf {
    runtime_dir().join("cowboyd.sock")
}

/// Persistent daemon state file (`$XDG_STATE_HOME/cowboy/daemon/state.json`,
/// else `~/.local/state/cowboy/daemon/state.json`).
pub fn state_path() -> PathBuf {
    let base = if let Some(d) = std::env::var_os("XDG_STATE_HOME").filter(|s| !s.is_empty()) {
        PathBuf::from(d)
    } else if let Some(h) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        PathBuf::from(h).join(".local/state")
    } else {
        runtime_dir().join("state")
    };
    base.join("cowboy/daemon/state.json")
}

// ---------------------------------------------------------------------------
// Persistent state
// ---------------------------------------------------------------------------

/// A worktree lease record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)] // fields consumed from M8 (leases)
pub struct Lease {
    pub session: SessionId,
    pub mode: LeaseMode,
    pub created_ms: u64,
    pub updated_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    sessions: BTreeMap<SessionId, SessionInfo>,
    /// Keyed by canonical worktree path (as a string).
    #[serde(default)]
    leases: BTreeMap<String, Lease>,
}

/// Daemon runtime state: the registry plus where it persists.
struct Daemon {
    state: State,
    state_path: PathBuf,
}

impl Daemon {
    fn load(state_path: PathBuf) -> Self {
        let state = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { state, state_path }
    }

    /// Persist the registry atomically (temp file + rename).
    #[allow(dead_code)] // called from M4 onward (session registration)
    fn save(&self) {
        if let Some(parent) = self.state_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(json) = serde_json::to_string_pretty(&self.state) else {
            return;
        };
        let tmp = self.state_path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &self.state_path);
        }
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Run the daemon until terminated. Refuses to start if another instance holds
/// the lock.
pub async fn serve() -> Result<()> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    // Single-instance lock (held for the process lifetime).
    let _lock = acquire_lock(&dir.join("cowboyd.lock"))?;

    let sock = socket_path();
    let _ = std::fs::remove_file(&sock); // we hold the lock, so any socket is stale
    let listener =
        UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    tracing::info!(sock = %sock.display(), "cowboyd listening");

    let daemon = Arc::new(Mutex::new(Daemon::load(state_path())));

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "accept error");
                continue;
            }
        };
        let daemon = daemon.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, daemon).await {
                tracing::debug!(error = %e, "connection ended");
            }
        });
    }
}

/// A held advisory lock file; released when dropped (process exit).
struct LockGuard {
    _file: std::fs::File,
}

fn acquire_lock(path: &Path) -> Result<LockGuard> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening lock {}", path.display()))?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        anyhow::bail!(
            "another cowboyd is already running (lock {})",
            path.display()
        );
    }
    Ok(LockGuard { _file: file })
}

async fn handle_conn(stream: UnixStream, daemon: Arc<Mutex<Daemon>>) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let Ok(req) = serde_json::from_str::<DaemonRequest>(line.trim()) else {
            continue; // ignore malformed
        };
        let resp = dispatch(req.req, &daemon).await;
        let out = DaemonResponse { id: req.id, resp };
        w.write_all(encode_line(&out).as_bytes()).await?;
        w.flush().await?;
    }
}

/// Handle one request. Milestones extend this match; unimplemented ops return
/// a clear error rather than panicking.
async fn dispatch(req: DaemonReq, daemon: &Mutex<Daemon>) -> DaemonResp {
    match req {
        DaemonReq::Ping => {
            let d = daemon.lock().await;
            DaemonResp::Pong {
                version: env!("CARGO_PKG_VERSION").to_string(),
                pid: std::process::id(),
                sessions: d.state.sessions.len(),
            }
        }
        DaemonReq::ListSessions { root } => {
            let d = daemon.lock().await;
            let sessions = d
                .state
                .sessions
                .values()
                .filter(|s| root.as_ref().is_none_or(|r| &s.root == r))
                .cloned()
                .collect();
            DaemonResp::Sessions { sessions }
        }
        other => DaemonResp::Err {
            message: format!("operation not implemented yet: {other:?}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Client helpers
// ---------------------------------------------------------------------------

/// Connect and issue a single request, returning the response. Errors if the
/// daemon is not reachable.
pub async fn request(req: DaemonReq) -> Result<DaemonResp> {
    let stream = UnixStream::connect(socket_path())
        .await
        .context("connecting to cowboyd (is it running?)")?;
    let (r, mut w) = stream.into_split();
    let env = DaemonRequest { id: 1, req };
    w.write_all(encode_line(&env).as_bytes()).await?;
    w.flush().await?;
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let resp: DaemonResponse = serde_json::from_str(line.trim()).context("parsing daemon reply")?;
    Ok(resp.resp)
}

/// Ensure a daemon is running: ping it, and if unreachable, spawn the `cowboyd`
/// binary (found next to the current exe), then wait for the socket.
pub async fn ensure_running() -> Result<()> {
    if matches!(request(DaemonReq::Ping).await, Ok(DaemonResp::Pong { .. })) {
        return Ok(());
    }
    // Spawn the cowboyd binary sitting next to us.
    let exe = std::env::current_exe().context("locating current exe")?;
    let cowboyd = exe.with_file_name("cowboyd");
    std::process::Command::new(&cowboyd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {}", cowboyd.display()))?;
    // Poll for readiness.
    for _ in 0..50 {
        if matches!(request(DaemonReq::Ping).await, Ok(DaemonResp::Pong { .. })) {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::bail!("cowboyd did not become ready")
}
