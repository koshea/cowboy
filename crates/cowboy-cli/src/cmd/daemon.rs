//! `cowboyd` — the local coordination daemon (control plane).
//!
//! Listens on a per-user Unix socket and maintains a persistent registry of
//! sessions and worktree leases. It does NOT host agent loops or sit in the
//! event-stream data path; sessions run as separate worker processes.
//!
//! This module also exposes the client-side helpers (`socket_path`, `request`,
//! `ensure_running`) that the `cowboy` CLI uses to reach the daemon.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use cowboy_core::daemonproto::{
    AttachTarget, BusMessage, DaemonReq, DaemonRequest, DaemonResp, DaemonResponse, LeaseMode,
    MsgTarget, SessionId, SessionInfo, SessionStatus,
};
use cowboy_core::netproto::encode_line;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
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
    /// Per-session message inboxes (the daemon-mediated coordination bus).
    #[serde(default)]
    inboxes: BTreeMap<SessionId, VecDeque<BusMessage>>,
}

/// Daemon runtime state: the registry plus where it persists.
struct Daemon {
    state: State,
    state_path: PathBuf,
    next_seq: u64,
    /// Ranch ids with an in-flight background advance. The bool is a "dirty"
    /// flag: set when another workstream finishes while an advance is running, so
    /// the coordinator re-runs once to pick up the late completion. Runtime-only.
    coordinating: std::collections::HashMap<String, bool>,
    /// The running web server (`cowboy web on`); `None` when not serving. Runtime-only
    /// — the setting itself lives in `web.yaml`.
    web: Option<WebServer>,
}

/// A spawned web server, and whether it is actually still up.
///
/// The liveness flag is the point. This used to be a bare cancellation token, set
/// *before* the server task was spawned and never cleared if that task failed — so a
/// bind that did not succeed (Tailscale down at daemon start, or the port already
/// taken) left the daemon believing it was serving. Two things then lied: `cowboy web
/// status` said "serving", and the idle-linger check kept the daemon alive forever for
/// a web UI that was not there. A `cowboyd` that outlives every session and serves
/// nothing is exactly the thing the linger exists to prevent.
struct WebServer {
    cancel: tokio_util::sync::CancellationToken,
    /// Cleared by the server task when it stops, however it stops.
    alive: Arc<std::sync::atomic::AtomicBool>,
}

use cowboy_core::time::now_ms;

/// A session is heartbeat-stale after this long without an update. Workers
/// heartbeat every 5s, so this tolerates several missed beats.
const STALE_AFTER_MS: u64 = 30_000;

/// How many terminal (completed/failed) session records the daemon retains before
/// pruning the oldest. Bounds unbounded growth from a long-lived daemon.
const MAX_TERMINAL_HISTORY: usize = 100;

/// Is a process alive? `kill(pid, 0)` succeeds for a live process we can signal
/// and fails with ESRCH if it's gone. Used for worker liveness.
fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Result of a [`Daemon::vacuum`] pass: the session ids removed from the registry.
///
/// It used to also carry the containers of crashed sessions for the caller to tear
/// down. Nothing to carry any more: a session's namespaces belong to a holder
/// process whose lifetime is tied to its worker's, so losing the worker *is* the
/// teardown and a crash needs no reaper.
struct VacuumOutcome {
    reaped: Vec<SessionId>,
}

/// Read the persisted registry, keeping a corrupt file rather than dropping it.
///
/// The old version was `read_to_string().ok().and_then(parse).ok().unwrap_or_default()`,
/// which treated "no such file" (a fresh daemon — normal) and "this file did not
/// parse" (something is wrong) identically. The second case is not cosmetic: `leases`
/// lives in here, and it is what stops two sessions running in one worktree. Losing it
/// silently means the next `cowboy` command happily starts a second session on top of
/// a live one.
///
/// Starting empty is still the only workable outcome — a daemon that refuses to start
/// leaves the user with no way to run anything — so the recovery is to say so loudly
/// and move the file aside so it can be inspected or restored by hand.
fn load_state(path: &Path) -> State {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return State::default(),
        Err(e) => {
            tracing::error!(
                path = %path.display(), error = %e,
                "could not read the daemon registry; starting empty (worktree leases are lost, \
                 so a busy worktree may not be detected until its session re-registers)"
            );
            return State::default();
        }
    };
    match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            let kept = path.with_extension(format!("json.corrupt-{}", now_ms()));
            let saved = std::fs::rename(path, &kept).is_ok();
            tracing::error!(
                path = %path.display(), error = %e, kept = %kept.display(), saved,
                "the daemon registry did not parse; starting empty. Worktree leases are lost, \
                 so a busy worktree may not be detected until its session re-registers"
            );
            State::default()
        }
    }
}

impl Daemon {
    fn load(state_path: PathBuf) -> Self {
        Self {
            state: load_state(&state_path),
            state_path,
            next_seq: 0,
            coordinating: std::collections::HashMap::new(),
            web: None,
        }
    }

    /// Start/stop the web server to match `web.yaml` (idempotent). Called on
    /// startup and on `DaemonReq::ReloadWeb`. Best-effort: a bad bind / missing
    /// token logs and leaves the server stopped rather than failing the daemon.
    fn apply_web(&mut self) {
        if let Some(w) = self.web.take() {
            w.cancel.cancel(); // stop the previous server (config changed / disabled)
        }
        let cfg = cowboy_core::config::WebConfig::load_global();
        if !cfg.enabled {
            return;
        }
        let addr = match crate::cmd::web::guard_bind_str(&cfg.bind, cfg.allow_lan) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "web UI enabled but bind rejected; not serving");
                return;
            }
        };
        if cfg.token.is_empty() {
            tracing::warn!("web UI enabled but no token in web.yaml; not serving");
            return;
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        self.web = Some(WebServer {
            cancel: cancel.clone(),
            alive: alive.clone(),
        });
        let token = cfg.token.clone();
        tokio::spawn(async move {
            let outcome = crate::cmd::web::serve_with(addr, token, cancel).await;
            // However it ended, it is no longer serving. Clearing this is what lets the
            // daemon exit when idle instead of lingering for a UI that is not there,
            // and what stops `cowboy web status` claiming it is up.
            alive.store(false, std::sync::atomic::Ordering::Relaxed);
            if let Err(e) = outcome {
                tracing::warn!(
                    error = %e, bind = %addr,
                    "web server exited; not serving the web UI (the daemon will now exit when \
                     idle). Fix the bind in web.yaml and run `cowboy web on` again."
                );
            }
        });
        tracing::info!(
            bind = %addr,
            "serving web UI; this daemon will stay up with no sessions (`cowboy web off` to \
             let it exit when idle)"
        );
    }

    /// Whether any session is still live. A *detached* session counts as live — the
    /// worker is still serving and the user can reattach — so the daemon must not
    /// exit under one.
    fn has_live_sessions(&self) -> bool {
        self.state
            .sessions
            .values()
            .any(|s| !s.status.is_terminal())
    }

    /// Whether the daemon is *actually* serving the web UI, in which case it is useful
    /// with no sessions at all and must not exit on idleness. Checks the server's
    /// liveness rather than the intent to start one — see [`WebServer`].
    fn serving_web(&self) -> bool {
        self.web
            .as_ref()
            .is_some_and(|w| w.alive.load(std::sync::atomic::Ordering::Relaxed))
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

    /// Reap already-marked `Stale` sessions and release their leases, then drop
    /// orphan leases (holder gone), bound terminal-session history, and remove
    /// dead per-session socket files. Idempotent maintenance run on startup and
    /// periodically so a crash never leaves a dangling lease that blocks new
    /// sessions, nor unbounded record/socket buildup. Caller sweeps first (so
    /// ranch coordination can see the just-stale records before they're reaped).
    /// Returns the reaped session ids.
    fn vacuum(&mut self) -> VacuumOutcome {
        // 1. Reap crashed sessions (already marked Stale by a prior sweep).
        let stale: Vec<SessionId> = self
            .state
            .sessions
            .iter()
            .filter(|(_, s)| s.status == SessionStatus::Stale)
            .map(|(id, _)| id.clone())
            .collect();
        let reaped: Vec<SessionId> = stale.clone();
        self.state.sessions.retain(|id, _| !reaped.contains(id));

        // 2. Drop leases whose holder no longer exists (reaped now, or orphaned by
        //    an earlier partial cleanup) — this is what frees a worktree after a crash.
        let live_ids: std::collections::HashSet<SessionId> =
            self.state.sessions.keys().cloned().collect();
        self.state
            .leases
            .retain(|_, l| live_ids.contains(&l.session));

        // 3. Bound terminal-session history (ids are ms-timestamp-prefixed, so a
        //    lexical sort is chronological — drop the oldest beyond the cap).
        let mut terminal: Vec<SessionId> = self
            .state
            .sessions
            .iter()
            .filter(|(_, s)| s.status.is_terminal())
            .map(|(id, _)| id.clone())
            .collect();
        if terminal.len() > MAX_TERMINAL_HISTORY {
            terminal.sort();
            let drop_n = terminal.len() - MAX_TERMINAL_HISTORY;
            for id in terminal.into_iter().take(drop_n) {
                self.state.sessions.remove(&id);
            }
        }

        self.save();

        VacuumOutcome { reaped }
    }

    /// Remove per-session socket files (`s-<id>.sock`) that don't belong to a
    /// currently-live session — leftovers from ended/crashed workers.
    ///
    /// `prune_unknown` governs sockets whose id isn't in our state *at all*. At
    /// startup these may belong to a worker that **survived a daemon restart**
    /// (e.g. one whose state was lost) and hasn't re-heartbeated yet — pruning it
    /// would strand a live worker with no client socket. So startup passes
    /// `false` (only reap sockets of sessions we *know* are terminal); the
    /// periodic vacuum passes `true`, by which point any survivor has
    /// re-registered and reads as live.
    fn prune_sockets(&self, prune_unknown: bool) {
        use std::collections::HashSet;
        let live: HashSet<&str> = self
            .state
            .sessions
            .iter()
            .filter(|(_, s)| !s.status.is_terminal())
            .map(|(id, _)| id.as_str())
            .collect();
        let known: HashSet<&str> = self.state.sessions.keys().map(String::as_str).collect();
        let Ok(entries) = std::fs::read_dir(runtime_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(id) = name
                .strip_prefix("s-")
                .and_then(|n| n.strip_suffix(".sock"))
            {
                if live.contains(id) {
                    continue; // a live worker — never prune its socket
                }
                // Otherwise the id is either a known-terminal session (always safe
                // to reap) or unknown to us (a possible restart survivor — only
                // reap once we're past the re-heartbeat window).
                if known.contains(id) || prune_unknown {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    /// Claim the coordinator slot for a ranch. Returns true if the caller should
    /// run an advance now; false if one is already in flight (in which case the
    /// slot is marked dirty so the running advance re-runs once when it finishes).
    fn claim_coordination(&mut self, ranch_id: &str) -> bool {
        if let Some(dirty) = self.coordinating.get_mut(ranch_id) {
            *dirty = true;
            false
        } else {
            self.coordinating.insert(ranch_id.to_string(), false);
            true
        }
    }

    /// Finish a coordination run. Returns true if it was marked dirty meanwhile
    /// and should re-run (the slot is kept); false if the slot is now free.
    fn finish_coordination(&mut self, ranch_id: &str) -> bool {
        match self.coordinating.get(ranch_id).copied() {
            Some(true) => {
                self.coordinating.insert(ranch_id.to_string(), false);
                true
            }
            _ => {
                self.coordinating.remove(ranch_id);
                false
            }
        }
    }

    /// Persist the registry atomically (temp file + rename).
    /// Persist the registry, atomically and durably.
    ///
    /// The temp-file-then-rename gets atomicity; the two fsyncs get durability, and
    /// without them the rename can reach the disk before the bytes do. The window is
    /// small but the payload is not: a truncated `state.json` is a *parse* failure,
    /// which used to reset the registry — leases included — without a word. So the
    /// two halves of this fix belong together.
    fn save(&self) {
        if let Some(parent) = self.state_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(json) = serde_json::to_string_pretty(&self.state) else {
            return;
        };
        let tmp = self.state_path.with_extension("json.tmp");
        if let Err(e) = write_durable(&tmp, json.as_bytes()) {
            tracing::warn!(path = %tmp.display(), error = %e, "could not stage the daemon registry");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.state_path) {
            tracing::warn!(path = %self.state_path.display(), error = %e, "could not save the daemon registry");
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        // The rename itself is a directory operation, so the directory needs its own
        // flush or the entry can be lost even though the file's contents are safe.
        if let Some(parent) = self.state_path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
    }
}

/// Write `bytes` to `path` and flush them to the device before returning.
fn write_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Arrange for this process to exit within `grace`, whatever else happens.
///
/// A detached thread rather than a task, so the runtime it backstops cannot starve it.
/// State is saved on every mutation, so there is nothing here that must be flushed
/// first.
fn force_exit_after(grace: std::time::Duration) {
    std::thread::Builder::new()
        .name("cowboyd-exit-watchdog".into())
        .spawn(move || {
            std::thread::sleep(grace);
            tracing::error!("shutdown did not complete within {grace:?}; exiting anyway");
            std::process::exit(0);
        })
        .ok();
}

/// How long the daemon stays up with nothing to supervise before exiting.
///
/// `COWBOY_DAEMON_LINGER` overrides it, in seconds; `0` keeps the daemon running
/// forever, which is what someone supervising it with systemd wants. A value that is
/// not a number is a typo, not a request to change the behaviour, so it warns and
/// uses the default rather than silently picking either extreme.
fn idle_linger() -> Option<std::time::Duration> {
    const DEFAULT_SECS: u64 = 20;
    let secs = match std::env::var("COWBOY_DAEMON_LINGER") {
        Ok(v) => v.trim().parse::<u64>().unwrap_or_else(|_| {
            tracing::warn!(
                value = %v,
                "COWBOY_DAEMON_LINGER is not a number of seconds; using {DEFAULT_SECS}"
            );
            DEFAULT_SECS
        }),
        Err(_) => DEFAULT_SECS,
    };
    (secs > 0).then(|| std::time::Duration::from_secs(secs))
}

/// Run the daemon until terminated. Refuses to start if another instance holds
/// the lock.
pub async fn serve() -> Result<()> {
    let dir = runtime_dir();
    // 0700 and verified ours: the sockets in here are a full control surface, and
    // without `XDG_RUNTIME_DIR` this is a predictable path in world-writable /tmp.
    crate::localsock::ensure_private_dir(&dir)?;

    // Single-instance lock (held for the process lifetime).
    let _lock = acquire_lock(&dir.join("cowboyd.lock"))?;

    let sock = socket_path();
    // Owner-only, and any socket here is stale because we hold the lock.
    let listener = crate::localsock::bind(&sock)?;
    tracing::info!(sock = %sock.display(), "cowboyd listening");

    let daemon = Arc::new(Mutex::new(Daemon::load(state_path())));

    // Fired by a `Shutdown` request (an upgraded CLI rolling a stale-version
    // daemon). Breaks the accept loop so `serve` returns and the process exits —
    // releasing the lock + socket — without touching the workers it spawned.
    let shutdown = tokio_util::sync::CancellationToken::new();

    // Reconcile on startup: any session whose worker pid is dead (or whose
    // worktree is gone) is marked Stale. Surviving workers re-heartbeat and
    // recover. Heartbeat age is ignored here (on-disk timestamps are old).
    {
        let mut d = daemon.lock().await;
        let newly = d.sweep_stale(false);
        // Only reap sockets of known-terminal sessions here; a worker that
        // survived this daemon's restart may not have re-heartbeated yet, so
        // leave unknown-id sockets for the periodic vacuum (post-recovery window).
        d.prune_sockets(false);
        if !newly.is_empty() {
            // Mark only; the periodic vacuum (below) reaps + tears down their
            // containers a tick later, giving any worker that survived the daemon
            // restart a chance to re-heartbeat and recover first.
            tracing::info!(?newly, "marked dead sessions stale on startup");
            d.save();
        }
        // Serve the web UI now if it's enabled in web.yaml (always-on setting).
        d.apply_web();
    }

    // Periodic staleness sweep so crashed/abandoned workers are noticed even
    // without a client poking the daemon.
    let sweeper = daemon.clone();
    let idle_shutdown = shutdown.clone();
    tokio::spawn(async move {
        // Tracks how long the daemon has had nothing to supervise.
        let mut idle_since: Option<std::time::Instant> = None;
        let linger = idle_linger();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
        // `interval` fires the first tick immediately; consume it so the first
        // sweep+reap runs a full period after startup, giving workers that
        // survived a daemon restart a window to re-heartbeat before being reaped.
        tick.tick().await;
        loop {
            tick.tick().await;
            let newly = {
                let mut d = sweeper.lock().await;
                let newly = d.sweep_stale(true);
                if !newly.is_empty() {
                    tracing::info!(?newly, "sessions went stale");
                    d.save();
                }
                newly
            };
            // Advance any ranch whose workstream just went stale — BEFORE the
            // vacuum reaps the record (coordination reads the session's ranch ids).
            for id in &newly {
                coordinate_after_terminal(&sweeper, id).await;
            }
            // Reap stale records + release dangling leases, prune sockets, bound
            // history. Idempotent; runs every tick so crashes self-heal.
            {
                let mut d = sweeper.lock().await;
                let outcome = d.vacuum();
                // Survivors have had their re-heartbeat window by now, so it's
                // safe to also reap sockets with no matching session record.
                d.prune_sockets(true);
                if !outcome.reaped.is_empty() {
                    tracing::info!(
                        reaped = ?outcome.reaped,
                        "reaped stale sessions (+released leases)"
                    );
                }
            }
            // Nothing to tear down for a crashed session: its namespaces, ruleset and
            // cgroup were owned by a holder process tied to the worker's lifetime, so
            // they went when the worker did. Only the empty cgroup *directory* can
            // linger, and `cowboy down` reaps those.

            // Exit once there is nothing left to supervise. The daemon auto-starts on
            // the next `cowboy` command, so lingering buys nothing but a process in
            // everyone's `ps` — and the surprise of finding one there after quitting
            // the last TUI. A short grace period rather than an immediate exit,
            // because a daemon that has just been auto-started sees zero sessions for
            // the moment before its client registers one.
            if let Some(window) = linger {
                let (live, web) = {
                    let d = sweeper.lock().await;
                    (d.has_live_sessions(), d.serving_web())
                };
                if live || web {
                    idle_since = None;
                } else {
                    let since = *idle_since.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed() >= window {
                        tracing::info!(
                            idle_for = ?since.elapsed(),
                            "no sessions left; cowboyd exiting (it restarts on the next command)"
                        );
                        idle_shutdown.cancel();
                        return;
                    }
                }
            }
        }
    });

    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "accept error");
                    continue;
                }
            },
            _ = shutdown.cancelled() => {
                tracing::info!("cowboyd shutting down (any remaining workers are left running)");
                // Same reasoning as the worker's watchdog: a shutdown that is logged
                // but does not happen is worse than no shutdown at all, because
                // everything downstream now believes the daemon is gone.
                force_exit_after(std::time::Duration::from_secs(10));
                break;
            }
        };
        let daemon = daemon.clone();
        let shutdown = shutdown.clone();
        // Who is on the other end, from the kernel. The socket is 0600 in a 0700
        // directory, but permissions can be widened by anything that touches the
        // file and this cannot: a peer that is not us never reaches `handle_conn`,
        // which would otherwise let them start sessions and drive workers.
        if !crate::localsock::peer_is_ours(&stream) {
            continue;
        }
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, daemon, shutdown).await {
                tracing::debug!(error = %e, "connection ended");
            }
        });
    }
    Ok(())
}

/// Host-only path for a worker's captured stdout/stderr: `<state>/cowboy/logs/
/// worker-<id>.log` (sibling of the daemon state dir — never under the workspace
/// mount, so the agent can't read it).
fn worker_log_path(id: &str) -> PathBuf {
    state_path()
        .parent() // .../cowboy/daemon
        .and_then(Path::parent) // .../cowboy
        .map(|p| p.join("logs"))
        .unwrap_or_else(|| runtime_dir().join("logs"))
        .join(format!("worker-{id}.log"))
}

/// The last few non-empty lines of a worker's log. Surfaced when a worker dies
/// before binding its socket so the daemon reports *why* (e.g. "run `cowboy
/// init` first") instead of an opaque "socket never appeared".
fn worker_log_tail(id: &str) -> Option<String> {
    let text = std::fs::read_to_string(worker_log_path(id)).ok()?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return None;
    }
    let start = lines.len().saturating_sub(20);
    Some(lines[start..].join("\n"))
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
    // Retry briefly: when an upgraded CLI rolls the daemon, this successor can
    // start while the predecessor is still releasing its lock. A short LOCK_NB
    // loop waits that handoff out (the OS frees the lock when the old process
    // exits) without blocking forever on a genuinely-live daemon.
    for attempt in 0..30 {
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(LockGuard { _file: file });
        }
        if attempt == 0 {
            tracing::debug!("cowboyd lock held; waiting for a predecessor to exit");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!(
        "another cowboyd is already running (lock {})",
        path.display()
    )
}

async fn handle_conn(
    stream: UnixStream,
    daemon: Arc<Mutex<Daemon>>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let out = match serde_json::from_str::<DaemonRequest>(line.trim()) {
            Ok(req) => DaemonResponse {
                id: req.id,
                resp: dispatch(req.req, &daemon, &shutdown).await,
            },
            // Reply with an error instead of silently dropping it, so the client
            // gets a clear failure rather than waiting (then timing out) for a
            // reply that never comes.
            Err(e) => DaemonResponse {
                id: 0,
                resp: DaemonResp::Err {
                    message: format!("malformed request: {e}"),
                },
            },
        };
        w.write_all(encode_line(&out).as_bytes()).await?;
        w.flush().await?;
    }
}

/// Handle one request. Milestones extend this match; unimplemented ops return
/// a clear error rather than panicking.
async fn dispatch(
    req: DaemonReq,
    daemon: &Arc<Mutex<Daemon>>,
    shutdown: &tokio_util::sync::CancellationToken,
) -> DaemonResp {
    match req {
        DaemonReq::Shutdown => {
            // Ack now, then cancel after a short grace so this response flushes
            // before the accept loop breaks and the process exits. Workers are
            // left running; they re-heartbeat into the successor daemon.
            let token = shutdown.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                token.cancel();
            });
            DaemonResp::ShuttingDown
        }
        DaemonReq::ReloadWeb => {
            let mut d = daemon.lock().await;
            d.apply_web();
            DaemonResp::Web {
                serving: d.web.is_some(),
            }
        }
        DaemonReq::WebStatus => {
            let d = daemon.lock().await;
            DaemonResp::Web {
                serving: d.serving_web(),
            }
        }
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
            ranch_id,
            workstream_id,
        } => {
            start_session(
                daemon,
                root,
                task,
                mode,
                force,
                resume,
                ranch_id,
                workstream_id,
            )
            .await
        }
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
            blocked_reason,
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
                    s.blocked_reason = blocked_reason;
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
            {
                let mut d = daemon.lock().await;
                if let Some(s) = d.state.sessions.get_mut(&id) {
                    s.status = SessionStatus::Completed;
                    s.worker_sock = None;
                }
                // A cleanly finished session frees its worktree immediately.
                d.release_all_for(&id);
                d.save();
            }
            // If this session ran a ranch workstream, advance the plan.
            coordinate_after_terminal(daemon, &id).await;
            DaemonResp::Completed
        }
        DaemonReq::FailSession { id, error } => {
            {
                let mut d = daemon.lock().await;
                if let Some(s) = d.state.sessions.get_mut(&id) {
                    s.status = SessionStatus::Failed;
                    s.worker_sock = None;
                    s.running_command = Some(format!("error: {error}"));
                }
                d.release_all_for(&id);
                d.save();
            }
            coordinate_after_terminal(daemon, &id).await;
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
        DaemonReq::SendMessage { to, from, event } => {
            let mut d = daemon.lock().await;
            let targets: Vec<SessionId> = match to {
                MsgTarget::Session(id) => vec![id],
                // Broadcast to every other known session.
                MsgTarget::All => d
                    .state
                    .sessions
                    .keys()
                    .filter(|id| **id != from)
                    .cloned()
                    .collect(),
            };
            let msg = BusMessage {
                from,
                ts_ms: now_ms(),
                event,
            };
            for id in &targets {
                d.state
                    .inboxes
                    .entry(id.clone())
                    .or_default()
                    .push_back(msg.clone());
            }
            d.save();
            DaemonResp::Sent {
                delivered: targets.len(),
            }
        }
        DaemonReq::GetInbox { session, drain } => {
            let mut d = daemon.lock().await;
            let messages: Vec<BusMessage> = if drain {
                d.state
                    .inboxes
                    .remove(&session)
                    .map(|q| q.into_iter().collect())
                    .unwrap_or_default()
            } else {
                d.state
                    .inboxes
                    .get(&session)
                    .map(|q| q.iter().cloned().collect())
                    .unwrap_or_default()
            };
            if drain {
                d.save();
            }
            DaemonResp::Inbox { messages }
        }
        DaemonReq::AcceptWorkstream { session, note: _ } => {
            accept_workstream(daemon, &session).await
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

// ---------------------------------------------------------------------------
// Ranch coordinator (background auto-advance)
// ---------------------------------------------------------------------------

/// Sign off on the ranch workstream a session is running (the user typed
/// `/accept`): mark the workstream complete + promote its artifacts via the exact
/// `cowboy ranch accept` CLI path, then advance the plan (launch newly-unblocked
/// workstreams, honoring `auto_advance`). The worker ends the session afterwards.
async fn accept_workstream(daemon: &Arc<Mutex<Daemon>>, session: &SessionId) -> DaemonResp {
    // Resolve the session's ranch + workstream + worktree from the registry.
    let (ranch_id, ws_id, worktree) = {
        let d = daemon.lock().await;
        match d.state.sessions.get(session) {
            Some(s) => match (s.ranch_id.clone(), s.workstream_id.clone()) {
                (Some(r), Some(w)) => (r, w, s.root.clone()),
                _ => {
                    return DaemonResp::Err {
                        message: "this session isn't a ranch workstream".into(),
                    }
                }
            },
            None => {
                return DaemonResp::Err {
                    message: format!("unknown session {session}"),
                }
            }
        }
    };
    let Ok(main_root) = crate::net::worktree::main_repo_root(&worktree) else {
        return DaemonResp::Err {
            message: "can't resolve the ranch's main repo".into(),
        };
    };
    // Mark the workstream complete + promote its artifacts (reuses `ranch accept`).
    let out = tokio::process::Command::new(worker_binary())
        .arg("ranch")
        .arg("accept")
        .arg(&ranch_id)
        .arg(&ws_id)
        .current_dir(&main_root)
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            return DaemonResp::Err {
                message: format!(
                    "ranch accept failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
            }
        }
        Err(e) => {
            return DaemonResp::Err {
                message: format!("running ranch accept: {e}"),
            }
        }
    }
    // Advance the plan: launch any newly-unblocked workstreams. Reuses the same
    // background coordinator the terminal-state path uses (claim guard + the
    // ranch's `auto_advance` preference).
    coordinate_after_terminal(daemon, session).await;
    DaemonResp::Accepted
}

/// When a ranch workstream's session reaches a terminal state, advance the plan
/// in the background: reconcile finished workstreams, promote their outputs, and
/// launch newly-ready ones — without the user re-running `cowboy ranch start`.
///
/// Mechanism: spawn `cowboy ranch start <ranch_id>` with its cwd set to the
/// ranch's main repo (derived from the finished session's worktree). That reuses
/// the exact, tested advance path and runs out-of-band, so it never deadlocks on
/// the daemon mutex. A per-ranch in-flight guard coalesces bursts; a dirty flag
/// re-runs once if another workstream finished mid-advance. Honors the ranch's
/// `auto_advance` flag and stops at acceptance gates (workstreams needing
/// sign-off don't unblock dependents).
async fn coordinate_after_terminal(daemon: &Arc<Mutex<Daemon>>, session: &SessionId) {
    // Resolve the finished session's ranch + worktree, then its main repo root.
    let (ranch_id, worktree) = {
        let d = daemon.lock().await;
        match d.state.sessions.get(session) {
            Some(s) => match &s.ranch_id {
                Some(rid) => (rid.clone(), s.root.clone()),
                None => return, // not part of a ranch
            },
            None => return,
        }
    };
    let Ok(main_root) = crate::net::worktree::main_repo_root(&worktree) else {
        tracing::debug!(%session, "coordinator: can't resolve main repo for worktree");
        return;
    };
    // Respect the ranch's auto-advance preference.
    match cowboy_core::ranch::load(&main_root, &ranch_id) {
        Ok(r) if !r.auto_advance => {
            tracing::debug!(ranch = %ranch_id, "coordinator: auto_advance disabled, skipping");
            return;
        }
        Ok(_) => {}
        Err(e) => {
            tracing::debug!(ranch = %ranch_id, error = %e, "coordinator: can't load ranch");
            return;
        }
    }

    // In-flight guard: if an advance is already running for this ranch, mark it
    // dirty (so it re-runs once more) and return; otherwise claim the slot.
    {
        let mut d = daemon.lock().await;
        if !d.claim_coordination(&ranch_id) {
            return;
        }
    }

    spawn_advance(daemon.clone(), ranch_id, main_root);
}

/// Spawn one `cowboy ranch start` advance for a ranch and, when it exits, either
/// clear the in-flight guard or re-run once if it was marked dirty meanwhile.
fn spawn_advance(daemon: Arc<Mutex<Daemon>>, ranch_id: String, main_root: PathBuf) {
    tokio::spawn(async move {
        tracing::info!(ranch = %ranch_id, "coordinator: advancing");
        let status = tokio::process::Command::new(worker_binary())
            .arg("ranch")
            .arg("start")
            .arg(&ranch_id)
            .current_dir(&main_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        if let Err(e) = status {
            tracing::warn!(ranch = %ranch_id, error = %e, "coordinator: advance failed to run");
        }
        // Decide whether to re-run (dirty) or finish.
        let rerun = {
            let mut d = daemon.lock().await;
            d.finish_coordination(&ranch_id)
        };
        if rerun {
            spawn_advance(daemon, ranch_id, main_root);
        }
    });
}

/// Spawn a worker process for a new session, supervise it, and return its
/// socket once it is listening.
#[allow(clippy::too_many_arguments)]
async fn start_session(
    daemon: &Arc<Mutex<Daemon>>,
    root: PathBuf,
    task: Option<String>,
    mode: LeaseMode,
    force: bool,
    resume: Option<String>,
    ranch_id: Option<String>,
    workstream_id: Option<String>,
) -> DaemonResp {
    let key = Daemon::lease_key(&root);
    // Acquire the worktree lease up front (exclusive sessions only). On conflict
    // bail before spawning so two writable workers never share a worktree.
    // Take the lease AND register the session in ONE critical section. Registering
    // after the spawn (as this used to) left a window in which the lease's holder
    // had no session record: `holder_state` reads that as `Gone`, so a concurrent
    // start could reclaim the worktree *without* `force` and spawn a second
    // exclusive worker onto it, and the vacuum's liveness sweep could drop the
    // lease outright. The pid is unknown until the child exists, so it's filled in
    // afterwards; the record is what makes the holder resolvable in the meantime.
    let sock;
    let journal;
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
        }
        sock = runtime_dir().join(format!("s-{id}.sock"));
        journal = root.join(".cowboy/sessions").join(&id).join("events.jsonl");
        d.state.sessions.insert(
            id.clone(),
            SessionInfo {
                id: id.clone(),
                root: root.clone(),
                task: task.clone(),
                status: SessionStatus::Starting,
                pid: None, // set once the child is spawned, below
                branch: None,
                session_name: None,
                worker_sock: Some(sock.clone()),
                journal_path: Some(journal.clone()),
                lease_mode: Some(mode),
                started_ms: now_ms(),
                last_heartbeat_ms: now_ms(),
                turn: 0,
                tokens: (0, 0),
                attached_clients: 0,
                diffstat: String::new(),
                running_command: None,
                blocked_reason: None,
                ranch_id: ranch_id.clone(),
                workstream_id: workstream_id.clone(),
            },
        );
        d.save();
        id
    };

    // Capture worker stdout/stderr to a host-only logfile (NOT under the workspace
    // mount, so the agent can't read it) instead of discarding it — otherwise a
    // worker that fails at startup (e.g. the control server can't bind) is
    // completely silent. Falls back to /dev/null if the log can't be opened.
    let log_path = worker_log_path(&id);
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let (out, err) = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => {
            let err = f.try_clone().map(std::process::Stdio::from);
            (
                std::process::Stdio::from(f),
                err.unwrap_or_else(|_| std::process::Stdio::null()),
            )
        }
        Err(e) => {
            tracing::warn!(path = %log_path.display(), error = %e, "worker log open failed; discarding output");
            (std::process::Stdio::null(), std::process::Stdio::null())
        }
    };

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
        .stdout(out)
        .stderr(err);
    if let Some(t) = &task {
        cmd.arg("--task").arg(t);
    }
    if let Some(r) = &resume {
        cmd.arg("--resume").arg(r);
    }
    if let Some(r) = &ranch_id {
        cmd.arg("--ranch-id").arg(r);
    }
    if let Some(w) = &workstream_id {
        cmd.arg("--workstream-id").arg(w);
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // Don't leave an orphan lease (or a phantom session) behind a worker
            // that never ran.
            let mut d = daemon.lock().await;
            d.release(&key, &id);
            d.state.sessions.remove(&id);
            d.save();
            return DaemonResp::Err {
                message: format!("spawning worker: {e}"),
            };
        }
    };
    let pid = child.id();

    {
        let mut d = daemon.lock().await;
        if let Some(s) = d.state.sessions.get_mut(&id) {
            s.pid = pid;
        }
        d.save();
    }

    // Supervise: when the child exits without a terminal message, mark stale.
    let sup = daemon.clone();
    let sup_id = id.clone();
    tokio::spawn(async move {
        let mut child = child;
        let _ = child.wait().await;
        let went_stale = {
            let mut d = sup.lock().await;
            let mut staled = false;
            if let Some(s) = d.state.sessions.get_mut(&sup_id) {
                if !s.status.is_terminal() {
                    s.status = SessionStatus::Stale;
                    s.worker_sock = None;
                    staled = true;
                }
            }
            d.save();
            staled
        };
        // A crashed ranch workstream should still advance the plan (so it's
        // reflected as failed and the user is prompted), mirroring clean exits.
        if went_stale {
            coordinate_after_terminal(&sup, &sup_id).await;
        }
    });

    // Wait for the worker to bind its socket. Bail out early — with the worker's
    // own error — if it exits first, rather than blocking for the full timeout.
    for _ in 0..100 {
        if sock.exists() {
            return DaemonResp::Started {
                id,
                worker_sock: sock,
            };
        }
        // The supervisor marks the session terminal (Stale) the moment the child
        // exits; a missing record means it never registered. Either way, stop.
        let gone = {
            let d = daemon.lock().await;
            d.state
                .sessions
                .get(&id)
                .map(|s| s.status.is_terminal())
                .unwrap_or(true)
        };
        if gone {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let message = match worker_log_tail(&id) {
        Some(tail) => format!("worker did not start:\n{tail}"),
        None => format!(
            "worker did not start (socket never appeared); see {}",
            worker_log_path(&id).display()
        ),
    };
    DaemonResp::Err { message }
}

// ---------------------------------------------------------------------------
// Client helpers
// ---------------------------------------------------------------------------

/// Connect and issue a single request, returning the response. Errors if the
/// daemon is not reachable.
pub async fn request(req: DaemonReq) -> Result<DaemonResp> {
    let stream = crate::localsock::connect(&socket_path())
        .await
        .context("connecting to cowboyd (is it running?)")?;
    let (r, mut w) = stream.into_split();
    let env = DaemonRequest { id: 1, req };
    w.write_all(encode_line(&env).as_bytes()).await?;
    w.flush().await?;
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    // Bound the wait: a wedged daemon (e.g. holding its state lock) must not hang
    // every CLI call and the worker heartbeat indefinitely. Daemon RPCs are all
    // quick state reads/writes, so 30s is generous.
    let n = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        reader.read_line(&mut line),
    )
    .await
    .context("timed out waiting for cowboyd reply (daemon wedged?)")??;
    if n == 0 {
        anyhow::bail!("cowboyd closed the connection without replying");
    }
    let resp: DaemonResponse = serde_json::from_str(line.trim()).context("parsing daemon reply")?;
    Ok(resp.resp)
}

/// Ensure a daemon of *this* version is running.
///
/// - Matching-version daemon already up → reuse it.
/// - A daemon of a **different** version is up (post-upgrade) → roll it: ask it
///   to shut down (its workers survive and re-heartbeat into the successor) and
///   start the new one. Set `COWBOY_NO_DAEMON_AUTORESTART` to refuse instead.
/// - No daemon → spawn one.
///
/// Rolling on version skew is what prevents a stale `cowboyd` (old protocol in
/// memory, spawning workers from the now-overwritten binary) from lingering
/// behind an upgraded CLI.
pub async fn ensure_running() -> Result<()> {
    match request(DaemonReq::Ping).await {
        Ok(DaemonResp::Pong { version, .. }) if version == env!("CARGO_PKG_VERSION") => Ok(()),
        Ok(DaemonResp::Pong { version, .. }) => {
            if std::env::var_os("COWBOY_NO_DAEMON_AUTORESTART").is_some() {
                anyhow::bail!(
                    "a cowboyd from version {version} is running but this CLI is {}; \
                     COWBOY_NO_DAEMON_AUTORESTART is set — stop cowboyd and retry",
                    env!("CARGO_PKG_VERSION")
                );
            }
            tracing::info!(
                daemon_version = %version,
                cli_version = env!("CARGO_PKG_VERSION"),
                "rolling stale-version cowboyd to match this CLI (workers survive)"
            );
            // Ask it to exit (best-effort), then wait for it to stop serving
            // before starting the successor — which clears the stale socket and
            // re-binds. The lock handoff is covered by acquire_lock's retry.
            let _ = request(DaemonReq::Shutdown).await;
            for _ in 0..50 {
                if !matches!(request(DaemonReq::Ping).await, Ok(DaemonResp::Pong { .. })) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            spawn_daemon_and_wait().await
        }
        _ => spawn_daemon_and_wait().await,
    }
}

/// Spawn the `cowboyd` binary sitting next to the current exe and poll until it
/// answers a matching-version `Ping`.
async fn spawn_daemon_and_wait() -> Result<()> {
    let exe = std::env::current_exe().context("locating current exe")?;
    let cowboyd = exe.with_file_name("cowboyd");
    std::process::Command::new(&cowboyd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {}", cowboyd.display()))?;
    for _ in 0..50 {
        if let Ok(DaemonResp::Pong { version, .. }) = request(DaemonReq::Ping).await {
            if version == env!("CARGO_PKG_VERSION") {
                return Ok(());
            }
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
            coordinating: std::collections::HashMap::new(),
            web: None,
        }
    }

    /// A registry that survives a round trip, and one that does not.
    ///
    /// The corrupt case has to start empty — a daemon that refuses to start leaves the
    /// user unable to run anything — but it must not do so *quietly*: `leases` is in
    /// here, and losing it means the next command may start a second session on top of
    /// a live one. So the file is kept for inspection rather than overwritten.
    #[test]
    fn a_corrupt_registry_is_preserved_rather_than_overwritten() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let path = tmp.path().join("state.json");

        // A good file round-trips, leases included.
        let mut d = Daemon {
            state: State::default(),
            state_path: path.clone(),
            next_seq: 0,
            coordinating: std::collections::HashMap::new(),
            web: None,
        };
        put_session(&mut d, "s1", SessionStatus::Running);
        d.save();
        assert!(load_state(&path).sessions.contains_key("s1"));

        // Truncated, as an un-fsynced write followed by a crash leaves it.
        std::fs::write(&path, "{\"sessions\": {\"s1\": ").unwrap();
        let recovered = load_state(&path);
        assert!(recovered.sessions.is_empty(), "it cannot be parsed");
        let kept: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("corrupt"))
            .collect();
        assert_eq!(
            kept.len(),
            1,
            "the unparseable registry must be kept for inspection, not dropped: {kept:?}"
        );
        assert!(!path.exists(), "and moved aside, not left to fail again");
    }

    /// A missing file is a fresh daemon, not a problem — it must not be confused with
    /// the case above, which is the confusion the old one-liner made.
    #[test]
    fn a_missing_registry_is_just_an_empty_one() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let path = tmp.path().join("state.json");
        assert!(load_state(&path).sessions.is_empty());
        assert!(
            std::fs::read_dir(tmp.path()).unwrap().flatten().count() == 0,
            "nothing is written for a daemon that has never saved"
        );
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
                session_name: None,
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
                blocked_reason: None,
                ranch_id: None,
                workstream_id: None,
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
    fn vacuum_reaps_stale_sessions_and_frees_their_leases() {
        let mut d = daemon();
        // A crashed session (Stale) holding an exclusive lease.
        put_session(&mut d, "crashed", SessionStatus::Stale);
        d.state.leases.insert(
            "/w".into(),
            Lease {
                session: "crashed".into(),
                mode: LeaseMode::Exclusive,
                created_ms: 0,
                updated_ms: 0,
            },
        );
        // A live session on the same root is left untouched.
        put_session(&mut d, "live", SessionStatus::Running);

        let outcome = d.vacuum();

        assert_eq!(outcome.reaped, vec!["crashed".to_string()]);
        assert!(!d.state.sessions.contains_key("crashed"), "stale reaped");
        assert!(d.state.sessions.contains_key("live"), "live kept");
        assert!(
            !d.state.leases.contains_key("/w"),
            "dangling lease freed so a new session can start"
        );
    }

    #[test]
    fn vacuum_drops_orphan_leases_and_caps_history() {
        let mut d = daemon();
        // Orphan lease: holder session no longer exists.
        d.state.leases.insert(
            "/orphan".into(),
            Lease {
                session: "ghost".into(),
                mode: LeaseMode::Exclusive,
                created_ms: 0,
                updated_ms: 0,
            },
        );
        // More terminal records than the cap; oldest (lexically smallest id) prune.
        for i in 0..(MAX_TERMINAL_HISTORY + 5) {
            put_session(&mut d, &format!("{i:06}-done"), SessionStatus::Completed);
        }

        d.vacuum();

        assert!(
            !d.state.leases.contains_key("/orphan"),
            "orphan lease dropped"
        );
        assert_eq!(
            d.state.sessions.len(),
            MAX_TERMINAL_HISTORY,
            "history capped"
        );
        assert!(
            !d.state.sessions.contains_key("000000-done"),
            "oldest pruned"
        );
        assert!(
            d.state
                .sessions
                .contains_key(&format!("{:06}-done", MAX_TERMINAL_HISTORY + 4)),
            "newest kept"
        );
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
    fn coordination_guard_coalesces_bursts_and_reruns_when_dirty() {
        let mut d = daemon();
        // First completion claims the slot and runs now.
        assert!(d.claim_coordination("billing"));
        // A second completion mid-advance does NOT run; it marks the slot dirty.
        assert!(!d.claim_coordination("billing"));
        assert!(!d.claim_coordination("billing"));
        // A different ranch is independent.
        assert!(d.claim_coordination("infra"));
        // The advance finishes: it was dirty, so it should re-run (slot kept).
        assert!(d.finish_coordination("billing"));
        // No further completions arrived during the re-run → slot frees.
        assert!(!d.finish_coordination("billing"));
        assert!(!d.coordinating.contains_key("billing"));
        // infra finishes clean (never dirtied) → frees immediately.
        assert!(!d.finish_coordination("infra"));
        assert!(d.coordinating.is_empty());
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
