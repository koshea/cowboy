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
    AttachTarget, DaemonReq, DaemonRequest, DaemonResp, DaemonResponse, LeaseMode, SessionId,
    SessionInfo, SessionStatus,
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

/// Liveness of a lease holder, deciding whether its lease can be reclaimed.
enum HolderState {
    /// No such session in the registry.
    Gone,
    /// Completed or Failed.
    Terminal,
    /// Marked `Stale` (worker died / heartbeat lapsed) — reclaim needs `--force`.
    Stale,
    /// Still running — never displaced.
    Live,
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
    next_seq: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A session is heartbeat-stale after this long without an update. Workers
/// heartbeat every 5s, so this tolerates several missed beats.
const STALE_AFTER_MS: u64 = 30_000;

/// Is a process alive? `kill(pid, 0)` succeeds for a live process we can signal
/// and fails with ESRCH if it's gone. Used for worker liveness.
fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

impl Daemon {
    fn load(state_path: PathBuf) -> Self {
        let state = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            state,
            state_path,
            next_seq: 0,
        }
    }

    /// Canonical lease key for a worktree root (matches the container/network
    /// path hashing). Falls back to the given path if it can't be canonicalized.
    fn lease_key(root: &Path) -> String {
        std::fs::canonicalize(root)
            .unwrap_or_else(|_| root.to_path_buf())
            .to_string_lossy()
            .into_owned()
    }

    /// Liveness of the session currently holding a lease. `Stale` is checked
    /// before the general terminal test (which also matches `Stale`) so a
    /// crashed-but-uncleaned session keeps requiring `--force` to displace.
    fn holder_state(&self, session: &SessionId) -> HolderState {
        match self.state.sessions.get(session) {
            None => HolderState::Gone,
            Some(s) if s.status == SessionStatus::Stale => HolderState::Stale,
            Some(s) if s.status.is_terminal() => HolderState::Terminal,
            Some(_) => HolderState::Live,
        }
    }

    /// Try to grant an exclusive lease on `key` to `session`. A lease held by a
    /// gone/terminal session is reclaimed automatically; a `Stale` one only with
    /// `force`; a live one is **never** displaced (rule 5). Returns the
    /// conflicting session id if denied.
    fn acquire(
        &mut self,
        key: &str,
        session: &str,
        mode: LeaseMode,
        force: bool,
    ) -> Result<(), SessionId> {
        if let Some(existing) = self.state.leases.get(key) {
            if existing.session != session {
                let reclaimable = match self.holder_state(&existing.session) {
                    HolderState::Gone | HolderState::Terminal => true,
                    HolderState::Stale => force,
                    HolderState::Live => false,
                };
                if !reclaimable {
                    return Err(existing.session.clone());
                }
            }
        }
        let now = now_ms();
        let created = self
            .state
            .leases
            .get(key)
            .filter(|l| l.session == session)
            .map(|l| l.created_ms)
            .unwrap_or(now);
        self.state.leases.insert(
            key.to_string(),
            Lease {
                session: session.to_string(),
                mode,
                created_ms: created,
                updated_ms: now,
            },
        );
        Ok(())
    }

    /// Release a lease iff it is held by `session` (a no-op otherwise, so a
    /// stale session can't release a stolen lease out from under its new owner).
    fn release(&mut self, key: &str, session: &str) {
        if self
            .state
            .leases
            .get(key)
            .is_some_and(|l| l.session == session)
        {
            self.state.leases.remove(key);
        }
    }

    /// Release any leases held by a session (called when it ends).
    fn release_all_for(&mut self, session: &str) {
        self.state.leases.retain(|_, l| l.session != session);
    }

    /// Mark crashed/abandoned sessions `Stale`. A non-terminal session is stale
    /// if its worker pid is dead, its worktree root has vanished, or (when
    /// `check_heartbeat`) it hasn't heartbeat within [`STALE_AFTER_MS`]. The
    /// heartbeat check is skipped on daemon startup, where on-disk timestamps are
    /// stale by construction (a surviving worker re-heartbeats within seconds).
    /// Stale sessions keep their lease (reclaim needs `--force` or `cleanup`).
    fn sweep_stale(&mut self, check_heartbeat: bool) -> Vec<SessionId> {
        let now = now_ms();
        let mut newly = Vec::new();
        for (id, s) in self.state.sessions.iter_mut() {
            if s.status.is_terminal() {
                continue; // already terminal/stale
            }
            // The daemon and every worker share a host, so a live pid is the
            // authoritative liveness signal; heartbeat age is only a fallback for
            // sessions with no pid to check (e.g. records restored from disk).
            let dead = match s.pid {
                Some(p) if pid_alive(p) => false,
                Some(_) => true,
                None => check_heartbeat && now.saturating_sub(s.last_heartbeat_ms) > STALE_AFTER_MS,
            };
            // A vanished worktree also means the session can't continue.
            if dead || !s.root.exists() {
                s.status = SessionStatus::Stale;
                s.worker_sock = None;
                newly.push(id.clone());
            }
        }
        newly
    }

    /// Reap `Stale` session records and release their leases. Returns the reaped
    /// session ids and the worktree keys whose lease was freed. Completed/Failed
    /// records are kept (intentional history). Never touches worktrees/branches.
    fn cleanup_stale(&mut self, dry_run: bool) -> (Vec<SessionId>, Vec<PathBuf>) {
        // Refresh staleness first (pid/root) so just-crashed sessions are caught
        // even between heartbeat sweeps.
        self.sweep_stale(true);
        let reap: Vec<SessionId> = self
            .state
            .sessions
            .iter()
            .filter(|(_, s)| s.status == SessionStatus::Stale)
            .map(|(id, _)| id.clone())
            .collect();
        let released: Vec<PathBuf> = self
            .state
            .leases
            .iter()
            .filter(|(_, l)| reap.contains(&l.session))
            .map(|(k, _)| PathBuf::from(k))
            .collect();
        if !dry_run {
            self.state.sessions.retain(|id, _| !reap.contains(id));
            self.state.leases.retain(|_, l| !reap.contains(&l.session));
            self.save();
        }
        (reap, released)
    }

    /// Persist the registry atomically (temp file + rename).
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

    // Reconcile on startup: any session whose worker pid is dead (or whose
    // worktree is gone) is marked Stale. Surviving workers re-heartbeat and
    // recover. Heartbeat age is ignored here (on-disk timestamps are old).
    {
        let mut d = daemon.lock().await;
        let reaped = d.sweep_stale(false);
        if !reaped.is_empty() {
            tracing::info!(?reaped, "marked dead sessions stale on startup");
            d.save();
        }
    }

    // Periodic staleness sweep so crashed/abandoned workers are noticed even
    // without a client poking the daemon.
    let sweeper = daemon.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            tick.tick().await;
            let mut d = sweeper.lock().await;
            let newly = d.sweep_stale(true);
            if !newly.is_empty() {
                tracing::info!(?newly, "sessions went stale");
                d.save();
            }
        }
    });

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

/// Path to the `cowboy` client binary (sibling of this `cowboyd`).
fn worker_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .map(|e| e.with_file_name("cowboy"))
        .unwrap_or_else(|| PathBuf::from("cowboy"))
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
async fn dispatch(req: DaemonReq, daemon: &Arc<Mutex<Daemon>>) -> DaemonResp {
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
        DaemonReq::GetSession { id } => {
            let d = daemon.lock().await;
            match d.state.sessions.get(&id) {
                Some(info) => DaemonResp::Session { info: info.clone() },
                None => DaemonResp::Err {
                    message: format!("no such session: {id}"),
                },
            }
        }
        DaemonReq::StartSession {
            root,
            task,
            mode,
            force,
            resume,
        } => start_session(daemon, root, task, mode, force, resume).await,
        DaemonReq::AcquireLease { key, session, mode } => {
            let mut d = daemon.lock().await;
            let k = Daemon::lease_key(&key);
            match d.acquire(&k, &session, mode, false) {
                Ok(()) => {
                    d.save();
                    DaemonResp::LeaseGranted { key }
                }
                Err(holder) => match d.state.sessions.get(&holder).cloned() {
                    Some(held_by) => DaemonResp::LeaseDenied { key, held_by },
                    None => DaemonResp::Err {
                        message: format!("worktree held by unknown session {holder}"),
                    },
                },
            }
        }
        DaemonReq::ReleaseLease { key, session } => {
            let mut d = daemon.lock().await;
            let k = Daemon::lease_key(&key);
            d.release(&k, &session);
            d.save();
            DaemonResp::Updated
        }
        DaemonReq::AttachSession { id } => {
            let d = daemon.lock().await;
            match d.state.sessions.get(&id) {
                None => DaemonResp::Err {
                    message: format!("no such session: {id}"),
                },
                Some(info) => {
                    let live = !info.status.is_terminal() && info.worker_sock.is_some();
                    if live {
                        DaemonResp::Attach {
                            target: AttachTarget::Live {
                                worker_sock: info.worker_sock.clone().unwrap(),
                            },
                        }
                    } else if let Some(journal) = info.journal_path.clone() {
                        DaemonResp::Attach {
                            target: AttachTarget::Replay {
                                journal_path: journal,
                                status: info.status,
                            },
                        }
                    } else {
                        DaemonResp::Err {
                            message: "session has no journal".into(),
                        }
                    }
                }
            }
        }
        DaemonReq::DetachSession { .. } => DaemonResp::Detached,
        DaemonReq::RegisterWorker { info } => {
            let mut d = daemon.lock().await;
            d.state.sessions.insert(info.id.clone(), info);
            d.save();
            DaemonResp::Registered
        }
        DaemonReq::UpdateSession {
            id,
            status,
            turn,
            tokens,
            diffstat,
            attached_clients,
            running_command,
            branch,
        } => {
            let mut d = daemon.lock().await;
            match d.state.sessions.get_mut(&id) {
                Some(s) => {
                    s.status = status;
                    s.turn = turn;
                    s.tokens = tokens;
                    s.diffstat = diffstat;
                    s.attached_clients = attached_clients;
                    s.running_command = running_command;
                    if branch.is_some() {
                        s.branch = branch;
                    }
                    s.last_heartbeat_ms = now_ms();
                    d.save();
                    DaemonResp::Updated
                }
                // The daemon forgot this session (restarted / cleaned). Tell the
                // worker so it can re-register.
                None => DaemonResp::Err {
                    message: format!("no such session: {id}"),
                },
            }
        }
        DaemonReq::Heartbeat { id, .. } => {
            let mut d = daemon.lock().await;
            if let Some(s) = d.state.sessions.get_mut(&id) {
                s.last_heartbeat_ms = now_ms();
            }
            DaemonResp::Updated
        }
        DaemonReq::CompleteSession { id } => {
            let mut d = daemon.lock().await;
            if let Some(s) = d.state.sessions.get_mut(&id) {
                s.status = SessionStatus::Completed;
                s.worker_sock = None;
            }
            // A cleanly finished session frees its worktree immediately.
            d.release_all_for(&id);
            d.save();
            DaemonResp::Completed
        }
        DaemonReq::FailSession { id, error } => {
            let mut d = daemon.lock().await;
            if let Some(s) = d.state.sessions.get_mut(&id) {
                s.status = SessionStatus::Failed;
                s.worker_sock = None;
                s.running_command = Some(format!("error: {error}"));
            }
            d.release_all_for(&id);
            d.save();
            DaemonResp::Failed
        }
        DaemonReq::CleanupStale { dry_run } => {
            let mut d = daemon.lock().await;
            let (reclaimed, leases_released) = d.cleanup_stale(dry_run);
            DaemonResp::CleanedUp {
                reclaimed,
                leases_released,
            }
        }
        // Worktree create/list are pure git/fs — no registry lock needed. List
        // is annotated with any session that holds each worktree's lease.
        DaemonReq::CreateWorktree { repo, branch, path } => {
            match crate::net::worktree::create(&repo, Some(&branch), path) {
                Ok((path, branch)) => DaemonResp::WorktreeCreated { path, branch },
                Err(e) => DaemonResp::Err {
                    message: e.to_string(),
                },
            }
        }
        DaemonReq::ListWorktrees { repo } => match crate::net::worktree::list(&repo) {
            Ok(mut list) => {
                let d = daemon.lock().await;
                for w in &mut list {
                    let key = Daemon::lease_key(&w.path);
                    if let Some(lease) = d.state.leases.get(&key) {
                        w.session = Some(lease.session.clone());
                        if let Some(s) = d.state.sessions.get(&lease.session) {
                            w.status = s.status;
                        }
                    }
                }
                DaemonResp::Worktrees { list }
            }
            Err(e) => DaemonResp::Err {
                message: e.to_string(),
            },
        },
    }
}

/// Spawn a worker process for a new session, supervise it, and return its
/// socket once it is listening.
async fn start_session(
    daemon: &Arc<Mutex<Daemon>>,
    root: PathBuf,
    task: Option<String>,
    mode: LeaseMode,
    force: bool,
    resume: Option<String>,
) -> DaemonResp {
    let key = Daemon::lease_key(&root);
    // Acquire the worktree lease up front (exclusive sessions only). On conflict
    // bail before spawning so two writable workers never share a worktree.
    let id = {
        let mut d = daemon.lock().await;
        d.next_seq += 1;
        let id = format!("{}-{}", now_ms(), d.next_seq);
        if mode == LeaseMode::Exclusive {
            if let Err(holder) = d.acquire(&key, &id, mode, force) {
                let held_by = d.state.sessions.get(&holder).cloned();
                return match held_by {
                    Some(held_by) => DaemonResp::LeaseDenied {
                        key: PathBuf::from(key),
                        held_by,
                    },
                    None => DaemonResp::Err {
                        message: format!("worktree held by unknown session {holder}"),
                    },
                };
            }
            d.save();
        }
        id
    };
    let sock = runtime_dir().join(format!("s-{id}.sock"));
    let journal = root.join(".cowboy/sessions").join(&id).join("events.jsonl");

    let mut cmd = tokio::process::Command::new(worker_binary());
    cmd.arg("x-session-worker")
        .arg("--root")
        .arg(&root)
        .arg("--id")
        .arg(&id)
        .arg("--sock")
        .arg(&sock)
        .arg("--register")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(t) = &task {
        cmd.arg("--task").arg(t);
    }
    if let Some(r) = &resume {
        cmd.arg("--resume").arg(r);
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // Don't leave an orphan lease behind a worker that never ran.
            let mut d = daemon.lock().await;
            d.release(&key, &id);
            d.save();
            return DaemonResp::Err {
                message: format!("spawning worker: {e}"),
            };
        }
    };
    let pid = child.id();

    {
        let mut d = daemon.lock().await;
        d.state.sessions.insert(
            id.clone(),
            SessionInfo {
                id: id.clone(),
                root: root.clone(),
                task,
                status: SessionStatus::Starting,
                pid,
                branch: None,
                container_name: None,
                worker_sock: Some(sock.clone()),
                journal_path: Some(journal),
                lease_mode: Some(mode),
                started_ms: now_ms(),
                last_heartbeat_ms: now_ms(),
                turn: 0,
                tokens: (0, 0),
                attached_clients: 0,
                diffstat: String::new(),
                running_command: None,
            },
        );
        d.save();
    }

    // Supervise: when the child exits without a terminal message, mark stale.
    let sup = daemon.clone();
    let sup_id = id.clone();
    tokio::spawn(async move {
        let mut child = child;
        let _ = child.wait().await;
        let mut d = sup.lock().await;
        if let Some(s) = d.state.sessions.get_mut(&sup_id) {
            if !s.status.is_terminal() {
                s.status = SessionStatus::Stale;
                s.worker_sock = None;
            }
        }
        d.save();
    });

    // Wait for the worker to bind its socket.
    for _ in 0..100 {
        if sock.exists() {
            return DaemonResp::Started {
                id,
                worker_sock: sock,
            };
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    DaemonResp::Err {
        message: "worker did not start (socket never appeared)".into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon() -> Daemon {
        Daemon {
            state: State::default(),
            state_path: PathBuf::from("/dev/null"),
            next_seq: 0,
        }
    }

    /// Register a session with a given status so its lease holder has known
    /// liveness.
    fn put_session(d: &mut Daemon, id: &str, status: SessionStatus) {
        d.state.sessions.insert(
            id.to_string(),
            SessionInfo {
                id: id.to_string(),
                root: PathBuf::from("/w"),
                task: None,
                status,
                pid: None,
                branch: None,
                container_name: None,
                worker_sock: None,
                journal_path: None,
                lease_mode: Some(LeaseMode::Exclusive),
                started_ms: 0,
                last_heartbeat_ms: 0,
                turn: 0,
                tokens: (0, 0),
                attached_clients: 0,
                diffstat: String::new(),
                running_command: None,
            },
        );
    }

    #[test]
    fn free_worktree_is_granted_and_reentrant() {
        let mut d = daemon();
        assert!(d.acquire("/w", "s1", LeaseMode::Exclusive, false).is_ok());
        // The same session re-acquiring is fine.
        assert!(d.acquire("/w", "s1", LeaseMode::Exclusive, false).is_ok());
    }

    #[test]
    fn live_holder_is_never_displaced_even_with_force() {
        let mut d = daemon();
        put_session(&mut d, "s1", SessionStatus::Running);
        d.acquire("/w", "s1", LeaseMode::Exclusive, false).unwrap();

        assert_eq!(
            d.acquire("/w", "s2", LeaseMode::Exclusive, false),
            Err("s1".to_string())
        );
        // Force does NOT steal a live lease (rule 5).
        assert_eq!(
            d.acquire("/w", "s2", LeaseMode::Exclusive, true),
            Err("s1".to_string())
        );
    }

    #[test]
    fn stale_holder_needs_force_to_steal() {
        let mut d = daemon();
        put_session(&mut d, "s1", SessionStatus::Stale);
        d.acquire("/w", "s1", LeaseMode::Exclusive, false).unwrap();

        // Without force a stale lease is reported as a conflict...
        assert_eq!(
            d.acquire("/w", "s2", LeaseMode::Exclusive, false),
            Err("s1".to_string())
        );
        // ...with force it is stolen.
        assert!(d.acquire("/w", "s2", LeaseMode::Exclusive, true).is_ok());
        assert_eq!(d.state.leases["/w"].session, "s2");
    }

    #[test]
    fn terminal_or_gone_holder_is_reclaimed_without_force() {
        // Completed holder: auto-reclaim.
        let mut d = daemon();
        put_session(&mut d, "s1", SessionStatus::Completed);
        d.acquire("/w", "s1", LeaseMode::Exclusive, false).unwrap();
        assert!(d.acquire("/w", "s2", LeaseMode::Exclusive, false).is_ok());

        // Gone holder (no session record): auto-reclaim too.
        let mut d2 = daemon();
        d2.state.leases.insert(
            "/w".into(),
            Lease {
                session: "ghost".into(),
                mode: LeaseMode::Exclusive,
                created_ms: 0,
                updated_ms: 0,
            },
        );
        assert!(d2.acquire("/w", "s9", LeaseMode::Exclusive, false).is_ok());
    }

    #[test]
    fn release_only_by_holder() {
        let mut d = daemon();
        d.acquire("/w", "s1", LeaseMode::Exclusive, false).unwrap();
        // A different session can't release someone else's lease.
        d.release("/w", "s2");
        assert!(d.state.leases.contains_key("/w"));
        // The holder can.
        d.release("/w", "s1");
        assert!(!d.state.leases.contains_key("/w"));
    }

    /// A pid that is essentially never alive, for dead-worker tests.
    const DEAD_PID: u32 = i32::MAX as u32;

    #[test]
    fn sweep_marks_dead_pid_and_missing_root_stale() {
        let live_root = assert_fs::TempDir::new().unwrap();
        let mut d = daemon();
        // Running but its worker pid is gone -> stale.
        put_session(&mut d, "dead", SessionStatus::Running);
        d.state.sessions.get_mut("dead").unwrap().pid = Some(DEAD_PID);
        d.state.sessions.get_mut("dead").unwrap().root = live_root.path().into();
        d.state.sessions.get_mut("dead").unwrap().last_heartbeat_ms = now_ms();
        // Running, our own pid (alive), but its worktree vanished -> stale.
        put_session(&mut d, "gone-root", SessionStatus::Running);
        d.state.sessions.get_mut("gone-root").unwrap().pid = Some(std::process::id());
        d.state.sessions.get_mut("gone-root").unwrap().root = PathBuf::from("/no/such/worktree");
        d.state
            .sessions
            .get_mut("gone-root")
            .unwrap()
            .last_heartbeat_ms = now_ms();
        // Running, alive pid, present root, fresh heartbeat -> stays live.
        put_session(&mut d, "ok", SessionStatus::Running);
        d.state.sessions.get_mut("ok").unwrap().pid = Some(std::process::id());
        d.state.sessions.get_mut("ok").unwrap().root = live_root.path().into();
        d.state.sessions.get_mut("ok").unwrap().last_heartbeat_ms = now_ms();

        let mut newly = d.sweep_stale(true);
        newly.sort();
        assert_eq!(newly, vec!["dead".to_string(), "gone-root".to_string()]);
        assert_eq!(d.state.sessions["ok"].status, SessionStatus::Running);
    }

    #[test]
    fn sweep_without_heartbeat_ignores_age() {
        let live_root = assert_fs::TempDir::new().unwrap();
        let mut d = daemon();
        // A pid-less record (e.g. restored from disk) with an ancient heartbeat:
        // only the heartbeat-aware sweep can judge it stale.
        put_session(&mut d, "old", SessionStatus::Running);
        d.state.sessions.get_mut("old").unwrap().pid = None;
        d.state.sessions.get_mut("old").unwrap().root = live_root.path().into();
        d.state.sessions.get_mut("old").unwrap().last_heartbeat_ms = 0; // ancient

        // Startup reconciliation (check_heartbeat=false) must not stale it.
        assert!(d.sweep_stale(false).is_empty());
        // The periodic sweep (heartbeat-aware) would.
        assert_eq!(d.sweep_stale(true), vec!["old".to_string()]);
    }

    #[test]
    fn sweep_spares_a_live_pid_despite_stale_heartbeat() {
        let live_root = assert_fs::TempDir::new().unwrap();
        let mut d = daemon();
        // Our own pid is alive; an ancient heartbeat must NOT mark it stale.
        put_session(&mut d, "busy", SessionStatus::Running);
        d.state.sessions.get_mut("busy").unwrap().pid = Some(std::process::id());
        d.state.sessions.get_mut("busy").unwrap().root = live_root.path().into();
        d.state.sessions.get_mut("busy").unwrap().last_heartbeat_ms = 0;
        assert!(d.sweep_stale(true).is_empty());
    }

    #[test]
    fn cleanup_reaps_stale_and_frees_lease_but_keeps_completed() {
        let mut d = daemon();
        // A stale session holding a lease.
        put_session(&mut d, "stale", SessionStatus::Stale);
        d.state.sessions.get_mut("stale").unwrap().pid = Some(DEAD_PID);
        d.acquire("/w", "stale", LeaseMode::Exclusive, false)
            .unwrap();
        // A completed session (history) — must be kept.
        put_session(&mut d, "done", SessionStatus::Completed);

        // Dry run changes nothing.
        let (reap, freed) = d.cleanup_stale(true);
        assert_eq!(reap, vec!["stale".to_string()]);
        assert_eq!(freed, vec![PathBuf::from("/w")]);
        assert!(d.state.sessions.contains_key("stale"));
        assert!(d.state.leases.contains_key("/w"));

        // Real run reaps the stale record + lease, keeps the completed one.
        let (reap, freed) = d.cleanup_stale(false);
        assert_eq!(reap, vec!["stale".to_string()]);
        assert_eq!(freed, vec![PathBuf::from("/w")]);
        assert!(!d.state.sessions.contains_key("stale"));
        assert!(!d.state.leases.contains_key("/w"));
        assert!(d.state.sessions.contains_key("done"));
    }

    #[test]
    fn release_all_for_drops_every_lease_of_a_session() {
        let mut d = daemon();
        d.acquire("/a", "s1", LeaseMode::Exclusive, false).unwrap();
        d.acquire("/b", "s1", LeaseMode::Exclusive, false).unwrap();
        d.acquire("/c", "s2", LeaseMode::Exclusive, false).unwrap();
        d.release_all_for("s1");
        assert!(!d.state.leases.contains_key("/a"));
        assert!(!d.state.leases.contains_key("/b"));
        assert!(d.state.leases.contains_key("/c"));
    }
}
