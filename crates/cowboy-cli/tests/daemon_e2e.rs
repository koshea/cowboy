//! End-to-end tests for the `cowboyd` daemon + worker + client stack, exercised
//! through the real `cowboy`/`cowboyd` binaries and unix sockets.
//!
//! All `#[ignore]`: they spawn real worker processes (and, for the turn test,
//! real Docker containers against a configured model). Run them explicitly:
//!
//! ```text
//! cargo test -p cowboy-cli --test daemon_e2e -- --ignored
//! ```
//!
//! Each test self-skips (prints why, returns Ok) when its prerequisites are
//! absent, so `--ignored` is safe to run anywhere. The "turn" test needs Docker,
//! the `cowboy/agent:local` image, and a model provider in `~/.config/cowboy`;
//! the rest only need a model *provider* to exist (the worker resolves one at
//! startup but, with no task, never calls it), so they supply a fake one.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use assert_fs::prelude::*;
use cowboy_core::daemonproto::{
    ClientMsg, DaemonReq, DaemonRequest, DaemonResp, DaemonResponse, LeaseMode, ServerMsg,
    SessionStatus, UiEventMsg,
};
use cowboy_core::netproto::{
    encode_line, ApprovalScope, GatewayMessage, HostMessage, NetworkAttempt, Protocol, Verdict,
};

// ---------------------------------------------------------------------------
// Prerequisites / skip helpers
// ---------------------------------------------------------------------------

fn docker_ok() -> bool {
    Command::new("docker")
        .args(["info"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// IDs of cowboy-labelled docker containers + networks (for snapshot-diff
/// cleanup so a test never leaks the worker's agent/gateway objects).
fn cowboy_docker_objects() -> (Vec<String>, Vec<String>) {
    let ids = |args: &[&str]| -> Vec<String> {
        Command::new("docker")
            .args(args)
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    (
        ids(&["ps", "-aq", "--filter", "label=cowboy=1"]),
        ids(&["network", "ls", "-q", "--filter", "label=cowboy=1"]),
    )
}

/// Remove any cowboy container/network created since `before`.
fn reap_new_docker(before: &(Vec<String>, Vec<String>)) {
    let (after_c, after_n) = cowboy_docker_objects();
    for id in after_c.iter().filter(|id| !before.0.contains(id)) {
        let _ = Command::new("docker").args(["rm", "-f", id]).output();
    }
    for id in after_n.iter().filter(|id| !before.1.contains(id)) {
        let _ = Command::new("docker").args(["network", "rm", id]).output();
    }
}

/// Does the user have a real model provider configured (for the turn test)?
fn real_provider() -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        roots.push(PathBuf::from(x));
    }
    if let Some(h) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        roots.push(PathBuf::from(h).join(".config"));
    }
    roots
        .into_iter()
        .map(|r| r.join("cowboy/providers.yaml"))
        .find(|p| p.is_file())
}

// ---------------------------------------------------------------------------
// Process + project helpers
// ---------------------------------------------------------------------------

/// Kill a child on drop so a failed test never leaks a process.
struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A throwaway temp dir of XDG paths + (optionally) a fake config home.
struct Env {
    runtime: assert_fs::TempDir,
    state: assert_fs::TempDir,
    /// Some when we supply a fake provider; None to inherit the real one.
    config: Option<assert_fs::TempDir>,
}

impl Env {
    /// XDG dirs with a fake provider + model (worker resolves it but, with no
    /// task, never calls it).
    fn fake() -> Self {
        let config = assert_fs::TempDir::new().unwrap();
        config
            .child("cowboy/providers.yaml")
            .write_str("version: 1\nproviders:\n  p:\n    base_url: https://x/v1\n    api_key: k\n")
            .unwrap();
        config
            .child("cowboy/models.yaml")
            .write_str("version: 1\ndefault: m\nmodels:\n  m:\n    provider: p\n    model: x\n")
            .unwrap();
        Self {
            runtime: assert_fs::TempDir::new().unwrap(),
            state: assert_fs::TempDir::new().unwrap(),
            config: Some(config),
        }
    }

    /// XDG dirs that inherit the real `~/.config` provider (for the turn test).
    fn real() -> Self {
        Self {
            runtime: assert_fs::TempDir::new().unwrap(),
            state: assert_fs::TempDir::new().unwrap(),
            config: None,
        }
    }

    fn sock(&self) -> PathBuf {
        self.runtime.path().join("cowboy/cowboyd.sock")
    }

    fn spawn_daemon(&self) -> Kill {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cowboyd"));
        cmd.env("XDG_RUNTIME_DIR", self.runtime.path())
            .env("XDG_STATE_HOME", self.state.path());
        if let Some(c) = &self.config {
            cmd.env("XDG_CONFIG_HOME", c.path());
        }
        Kill(cmd.spawn().expect("spawn cowboyd"))
    }
}

/// A fresh git project with `.cowboy/` config (via `cowboy init`).
fn make_project() -> assert_fs::TempDir {
    let dir = assert_fs::TempDir::new().unwrap();
    let _ = Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .arg("init")
        .arg("-q")
        .status();
    let ok = Command::new(env!("CARGO_BIN_EXE_cowboy"))
        .current_dir(dir.path())
        .arg("init")
        .output()
        .expect("run cowboy init")
        .status
        .success();
    assert!(ok, "cowboy init should succeed");
    // An initial commit so `git worktree add` (ranch start) has a base HEAD.
    let git = |args: &[&str]| {
        let _ = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(args)
            .output();
    };
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    dir
}

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

/// One blocking daemon request/response.
fn dreq(sock: &Path, req: DaemonReq) -> Option<DaemonResp> {
    let stream = UnixStream::connect(sock).ok()?;
    let mut w = stream.try_clone().ok()?;
    w.write_all(encode_line(&DaemonRequest { id: 1, req }).as_bytes())
        .ok()?;
    w.flush().ok()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str::<DaemonResponse>(line.trim())
        .ok()
        .map(|r| r.resp)
}

fn wait_pong(sock: &Path) -> bool {
    for _ in 0..50 {
        if matches!(dreq(sock, DaemonReq::Ping), Some(DaemonResp::Pong { .. })) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn start(sock: &Path, root: &Path, task: Option<&str>) -> DaemonResp {
    dreq(
        sock,
        DaemonReq::StartSession {
            root: root.to_path_buf(),
            task: task.map(str::to_string),
            mode: LeaseMode::Exclusive,
            force: false,
            resume: None,
            ranch_id: None,
            workstream_id: None,
        },
    )
    .expect("daemon reachable")
}

fn get(sock: &Path, id: &str) -> Option<cowboy_core::daemonproto::SessionInfo> {
    match dreq(sock, DaemonReq::GetSession { id: id.to_string() }) {
        Some(DaemonResp::Session { info }) => Some(info),
        _ => None,
    }
}

/// A client connection to a worker's per-session socket.
struct Client {
    r: BufReader<UnixStream>,
    w: UnixStream,
}
impl Client {
    fn connect(sock: &Path) -> Self {
        let s = UnixStream::connect(sock).expect("connect worker socket");
        s.set_read_timeout(Some(Duration::from_secs(180))).unwrap();
        let w = s.try_clone().unwrap();
        Self {
            r: BufReader::new(s),
            w,
        }
    }
    fn send(&mut self, msg: &ClientMsg) {
        self.w.write_all(encode_line(msg).as_bytes()).unwrap();
        self.w.flush().unwrap();
    }
    fn hello(&mut self, since_seq: Option<u64>) {
        self.send(&ClientMsg::Hello {
            since_seq,
            read_only: false,
        });
    }
    fn recv(&mut self) -> Option<ServerMsg> {
        let mut line = String::new();
        match self.r.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => serde_json::from_str(line.trim()).ok(),
        }
    }
}

/// Compute a worker's host control-socket path (matches the gateway).
fn control_sock_for(root: &Path) -> PathBuf {
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let hash = cowboy_cli::net::runtime::project_hash(&canon);
    std::env::temp_dir()
        .join("cowboy-run")
        .join(format!("control-{hash:08x}.sock"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Two `cowboy` invocations in the same worktree: the daemon refuses the second
/// (its worktree lease is held by the live first session).
#[test]
#[ignore = "spawns real worker processes"]
fn e2e_same_worktree_collision_is_denied() {
    let env = Env::fake();
    let _d = env.spawn_daemon();
    let sock = env.sock();
    assert!(wait_pong(&sock));
    let proj = make_project();

    let (id1, ws1) = match start(&sock, proj.path(), None) {
        DaemonResp::Started { id, worker_sock } => (id, worker_sock),
        other => panic!("expected Started, got {other:?}"),
    };
    assert!(ws1.exists(), "first worker should bind its socket");

    // Second start in the same worktree is denied, naming the live holder.
    match start(&sock, proj.path(), None) {
        DaemonResp::LeaseDenied { held_by, .. } => assert_eq!(held_by.id, id1),
        other => panic!("expected LeaseDenied, got {other:?}"),
    }

    // Wind down the first session.
    Client::connect(&ws1).send(&ClientMsg::End);
    std::thread::sleep(Duration::from_millis(500));
}

/// A network approval crosses the worker: the gateway asks over the control
/// socket, the attached client allows, the gateway gets the verdict. With no
/// client attached the same ask is denied (fail closed).
#[test]
#[ignore = "spawns real worker processes"]
fn e2e_approval_routes_through_worker_and_fails_closed() {
    let env = Env::fake();
    let _d = env.spawn_daemon();
    let sock = env.sock();
    assert!(wait_pong(&sock));
    let proj = make_project();

    let ws = match start(&sock, proj.path(), None) {
        DaemonResp::Started { worker_sock, .. } => worker_sock,
        other => panic!("expected Started, got {other:?}"),
    };

    // The worker binds the host control socket shortly after starting.
    let ctrl = control_sock_for(proj.path());
    let mut gw = None;
    for _ in 0..100 {
        if let Ok(s) = UnixStream::connect(&ctrl) {
            gw = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let gw = gw.expect("worker control socket should appear");
    gw.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut gw_w = gw.try_clone().unwrap();
    let mut gw_r = BufReader::new(gw);

    let attempt = NetworkAttempt {
        protocol: Protocol::Tls,
        host: Some("example.com".into()),
        ip: None,
        port: 443,
    };

    // With a client attached, the ask is routed and the client's verdict wins.
    let mut client = Client::connect(&ws);
    client.hello(None);
    assert!(matches!(client.recv(), Some(ServerMsg::Snapshot { .. })));
    gw_w.write_all(
        encode_line(&GatewayMessage::Ask {
            id: 1,
            attempt: attempt.clone(),
        })
        .as_bytes(),
    )
    .unwrap();

    // Client sees the approval prompt and allows it.
    let approval_id = loop {
        match client.recv() {
            Some(ServerMsg::Approval { id, dest }) => {
                assert_eq!(dest, "example.com:443");
                break id;
            }
            Some(_) => continue,
            None => panic!("client never received the approval prompt"),
        }
    };
    client.send(&ClientMsg::ApprovalReply {
        id: approval_id,
        verdict: Verdict::Allow,
        scope: ApprovalScope::Session,
    });

    let mut line = String::new();
    gw_r.read_line(&mut line).unwrap();
    match serde_json::from_str::<HostMessage>(line.trim()).unwrap() {
        HostMessage::Decision { id, verdict, .. } => {
            assert_eq!(id, 1);
            assert_eq!(verdict, Verdict::Allow);
        }
    }

    // Drop the client; with zero approvers the next ask fails closed.
    drop(client);
    std::thread::sleep(Duration::from_millis(300));
    gw_w.write_all(encode_line(&GatewayMessage::Ask { id: 2, attempt }).as_bytes())
        .unwrap();
    line.clear();
    gw_r.read_line(&mut line).unwrap();
    match serde_json::from_str::<HostMessage>(line.trim()).unwrap() {
        HostMessage::Decision { id, verdict, .. } => {
            assert_eq!(id, 2);
            assert_eq!(verdict, Verdict::Deny, "zero-client ask must fail closed");
        }
    }

    Client::connect(&ws).send(&ClientMsg::End);
    std::thread::sleep(Duration::from_millis(500));
}

/// Killing a worker out from under the daemon marks the session `Stale`; then
/// `cleanup` reaps the record and frees its lease.
#[test]
#[ignore = "spawns real worker processes"]
fn e2e_kill_worker_marks_stale_then_cleanup_reaps() {
    let env = Env::fake();
    let _d = env.spawn_daemon();
    let sock = env.sock();
    assert!(wait_pong(&sock));
    let proj = make_project();

    let (id, _ws) = match start(&sock, proj.path(), None) {
        DaemonResp::Started { id, worker_sock } => (id, worker_sock),
        other => panic!("expected Started, got {other:?}"),
    };
    let pid = get(&sock, &id).and_then(|s| s.pid).expect("worker pid");

    // Kill the worker; the daemon (its parent) notices and marks it Stale.
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
    let mut stale = false;
    for _ in 0..50 {
        if get(&sock, &id).map(|s| s.status) == Some(SessionStatus::Stale) {
            stale = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(stale, "worker death should mark the session Stale");

    // Cleanup reaps the stale record + lease.
    match dreq(&sock, DaemonReq::CleanupStale { dry_run: false }) {
        Some(DaemonResp::CleanedUp { reclaimed, .. }) => {
            assert!(reclaimed.contains(&id), "cleanup should reap {id}");
        }
        other => panic!("expected CleanedUp, got {other:?}"),
    }
    assert!(get(&sock, &id).is_none(), "reaped session should be gone");
}

/// A worker outlives a daemon restart (even one that lost its state) and is
/// re-adopted when its heartbeat re-registers it.
#[test]
#[ignore = "spawns real worker processes; ~10s heartbeat wait"]
fn e2e_daemon_restart_readopts_worker() {
    let env = Env::fake();
    let sock = env.sock();
    let proj = make_project();

    // Start a session under the first daemon.
    let d1 = env.spawn_daemon();
    assert!(wait_pong(&sock));
    let (id, ws) = match start(&sock, proj.path(), None) {
        DaemonResp::Started { id, worker_sock } => (id, worker_sock),
        other => panic!("expected Started, got {other:?}"),
    };

    // Kill the daemon and wipe its state — the worker survives (reparented).
    drop(d1);
    std::thread::sleep(Duration::from_millis(300));
    let _ = std::fs::remove_file(env.state.path().join("cowboy/daemon/state.json"));

    // Restart with empty state; the worker's heartbeat should re-register it.
    let _d2 = env.spawn_daemon();
    assert!(wait_pong(&sock));
    let mut readopted = false;
    for _ in 0..120 {
        if get(&sock, &id).is_some() {
            readopted = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        readopted,
        "worker should re-register after a daemon restart"
    );

    Client::connect(&ws).send(&ClientMsg::End);
    std::thread::sleep(Duration::from_millis(500));
}

/// Foundations against a real model: a single turn must drive the coordination
/// tools (artifact, blocked/unblock, handoff) and leave the right on-disk
/// effects — the regression check for prompt/model compatibility. Asserts the
/// file effects (robust to wording), not the transcript. Needs a model provider
/// but not Docker (these tools run host-side; the task does no shell/network).
#[test]
#[ignore = "real model: needs a provider in ~/.config/cowboy"]
fn e2e_foundation_tools_record_artifacts_lifecycle_handoff() {
    let Some(_) = real_provider() else {
        eprintln!("skipping: no model provider in ~/.config/cowboy");
        return;
    };

    let docker_before = cowboy_docker_objects();
    let env = Env::real();
    let _d = env.spawn_daemon();
    let sock = env.sock();
    assert!(wait_pong(&sock));
    let proj = make_project();

    let task = "Do exactly these steps using your tools, with NO shell commands: \
        (1) artifact tool: publish kind=contract, title=\"API Contract\", \
        content=\"# API\\nGET /things\"; \
        (2) blocked tool with reason \"need a design review\", then the unblock tool; \
        (3) handoff tool: goal=\"demo\", status=\"complete\", next_steps=\"wire the API\"; \
        (4) final with a one-line summary.";
    let (id, ws) = match start(&sock, proj.path(), Some(task)) {
        DaemonResp::Started { id, worker_sock } => (id, worker_sock),
        other => panic!("expected Started, got {other:?}"),
    };

    // Drive the turn to completion.
    let mut a = Client::connect(&ws);
    a.hello(None);
    loop {
        match a.recv() {
            Some(ServerMsg::Event {
                event: UiEventMsg::TurnDone,
                ..
            }) => break,
            Some(ServerMsg::Ended { .. }) | None => break,
            Some(_) => {}
        }
    }

    let sd = proj.path().join(".cowboy/sessions").join(&id);
    let lifecycle = std::fs::read_to_string(sd.join("lifecycle.jsonl")).unwrap_or_default();
    let artifacts = std::fs::read_to_string(sd.join("artifacts.jsonl")).unwrap_or_default();
    let handoff = std::fs::read_to_string(sd.join("handoff.md")).unwrap_or_default();

    // End the session so finalize runs (emits session_completed), then re-read.
    a.send(&ClientMsg::End);
    std::thread::sleep(Duration::from_millis(800));
    let lifecycle_final = std::fs::read_to_string(sd.join("lifecycle.jsonl")).unwrap_or_default();

    let _ = Command::new(env!("CARGO_BIN_EXE_cowboy"))
        .current_dir(proj.path())
        .arg("down")
        .output();
    // The worker may eagerly bring up its agent/gateway even for a tool-only
    // task; reap anything new so the suite never leaks containers/networks.
    reap_new_docker(&docker_before);

    // Each coordination tool left its mark.
    for needle in [
        "artifact_published",
        "\"blocked\"",
        "unblocked",
        "handoff_created",
    ] {
        assert!(
            lifecycle.contains(needle),
            "lifecycle.jsonl should record {needle}; got:\n{lifecycle}"
        );
    }
    assert!(
        artifacts.contains("\"contract\"") && artifacts.contains("\"handoff\""),
        "a contract + handoff artifact should be indexed; got:\n{artifacts}"
    );
    assert!(
        handoff.to_lowercase().contains("demo"),
        "handoff.md should capture the goal; got:\n{handoff}"
    );
    assert!(
        lifecycle_final.contains("session_completed"),
        "ending the session should emit session_completed"
    );
}

/// Ranch Stage 2: `cowboy ranch start` launches the ready workstream (schema) in
/// its own worktree/branch, tags its session, and leaves the dependent one (api)
/// waiting — the dependency-aware launch loop. Needs a provider; cleans up its
/// worktree + any containers.
#[test]
#[ignore = "real model: launches a ranch workstream worker"]
fn e2e_ranch_start_launches_ready_workstream() {
    let Some(_) = real_provider() else {
        eprintln!("skipping: no model provider in ~/.config/cowboy");
        return;
    };
    let docker_before = cowboy_docker_objects();
    let env = Env::real();
    let _d = env.spawn_daemon();
    let sock = env.sock();
    assert!(wait_pong(&sock));
    let proj = make_project();

    // Seed a ranch: schema (no deps) + api (depends on schema).
    let ranch_dir = proj.path().join(".cowboy/ranches/billing");
    std::fs::create_dir_all(&ranch_dir).unwrap();
    std::fs::write(
        ranch_dir.join("ranch.yaml"),
        "version: 1\nid: billing\ntitle: Billing\nstatus: planning\n\
         created_ms: 1\nupdated_ms: 1\nworkstreams:\n\
         \x20 - id: schema\n    title: Schema\n    goal: write hello to a file\n    depends_on: []\n\
         \x20 - id: api\n    title: API\n    depends_on: [schema]\n",
    )
    .unwrap();

    // Launch via the real CLI against the test daemon.
    let out = Command::new(env!("CARGO_BIN_EXE_cowboy"))
        .current_dir(proj.path())
        .env("XDG_RUNTIME_DIR", env.runtime.path())
        .env("XDG_STATE_HOME", env.state.path())
        .args(["ranch", "start", "billing"])
        .output()
        .expect("run ranch start");
    assert!(
        out.status.success(),
        "ranch start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A session tagged ranch=billing / workstream=schema is registered.
    let mut tagged = None;
    for _ in 0..50 {
        if let Some(DaemonResp::Sessions { sessions }) =
            dreq(&sock, DaemonReq::ListSessions { root: None })
        {
            if let Some(s) = sessions
                .into_iter()
                .find(|s| s.workstream_id.as_deref() == Some("schema"))
            {
                tagged = Some(s);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let tagged = tagged.expect("schema workstream session should be registered");
    assert_eq!(tagged.ranch_id.as_deref(), Some("billing"));

    // ranch.yaml advanced: schema running on its branch; api still only declared.
    let yaml = std::fs::read_to_string(ranch_dir.join("ranch.yaml")).unwrap();
    assert!(
        yaml.contains("cowboy/billing-schema"),
        "schema branch recorded:\n{yaml}"
    );
    let branch_ok = Command::new("git")
        .arg("-C")
        .arg(proj.path())
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/heads/cowboy/billing-schema",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(branch_ok, "branch cowboy/billing-schema should exist");

    // Cleanup: end the worker, remove its worktree, reap any containers.
    if let Some(ws) = &tagged.worker_sock {
        Client::connect(ws).send(&ClientMsg::End);
    }
    std::thread::sleep(Duration::from_millis(700));
    if let Some(p) = cowboy_core::ranch::load(proj.path(), "billing")
        .ok()
        .and_then(|r| r.workstream("schema").and_then(|w| w.worktree_path.clone()))
    {
        let _ = Command::new("git")
            .arg("-C")
            .arg(proj.path())
            .args(["worktree", "remove", "--force"])
            .arg(&p)
            .output();
    }
    reap_new_docker(&docker_before);
}

/// Ranch coordinator (auto-advance): start a ranch's first workstream, let it
/// actually run to completion in Docker, and assert the daemon AUTO-launches the
/// dependent workstream — without a second manual `ranch start`. This exercises
/// the full loop: one-shot worker finishes → CompleteSession → coordinator spawns
/// an advance → api launches. Needs Docker + a real model.
#[test]
#[ignore = "real Docker + model: exercises background auto-advance"]
fn e2e_ranch_coordinator_auto_advances_to_dependent() {
    if !docker_ok() {
        eprintln!("skipping: docker not available");
        return;
    }
    let Some(_) = real_provider() else {
        eprintln!("skipping: no model provider in ~/.config/cowboy");
        return;
    };
    let docker_before = cowboy_docker_objects();
    let env = Env::real();
    let _d = env.spawn_daemon();
    let sock = env.sock();
    assert!(wait_pong(&sock));
    let proj = make_project();

    // schema does a trivial, quickly-completable task; api depends on it. No
    // acceptance criteria / expected artifacts on schema, so it auto-completes
    // (the acceptance gate would otherwise pause for sign-off).
    let ranch_dir = proj.path().join(".cowboy/ranches/billing");
    std::fs::create_dir_all(&ranch_dir).unwrap();
    std::fs::write(
        ranch_dir.join("ranch.yaml"),
        "version: 1\nid: billing\ntitle: Billing\nstatus: planning\nauto_advance: true\n\
         created_ms: 1\nupdated_ms: 1\nworkstreams:\n\
         \x20 - id: schema\n    title: Schema\n    goal: Create a file hello.txt containing the word hello, then finish.\n    depends_on: []\n\
         \x20 - id: api\n    title: API\n    goal: Create a file api.txt containing the word api, then finish.\n    depends_on: [schema]\n",
    )
    .unwrap();

    // Launch only the ready workstream (schema). We never call start again.
    let out = Command::new(env!("CARGO_BIN_EXE_cowboy"))
        .current_dir(proj.path())
        .env("XDG_RUNTIME_DIR", env.runtime.path())
        .env("XDG_STATE_HOME", env.state.path())
        .args(["ranch", "start", "billing"])
        .output()
        .expect("run ranch start");
    assert!(
        out.status.success(),
        "ranch start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Wait (generously) for the coordinator to launch a session tagged
    // workstream=api — proof the dependent advanced automatically.
    let mut api = None;
    for _ in 0..1200 {
        if let Some(DaemonResp::Sessions { sessions }) =
            dreq(&sock, DaemonReq::ListSessions { root: None })
        {
            if let Some(s) = sessions
                .into_iter()
                .find(|s| s.workstream_id.as_deref() == Some("api"))
            {
                api = Some(s);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let api = api.expect("coordinator should auto-launch the api workstream");
    assert_eq!(api.ranch_id.as_deref(), Some("billing"));

    // ranch.yaml reflects schema done + api launched on its branch.
    let r = cowboy_core::ranch::load(proj.path(), "billing").unwrap();
    assert!(
        r.workstream("schema").unwrap().status.is_done(),
        "schema should be complete: {:?}",
        r.workstream("schema").unwrap().status
    );

    // Cleanup: end any live workstream worker, remove worktrees, reap containers.
    for wsid in ["schema", "api"] {
        if let Some(w) = r.workstream(wsid) {
            if let Some(sid) = &w.session_id {
                if let Some(info) = get(&sock, sid) {
                    if let Some(ws) = &info.worker_sock {
                        Client::connect(ws).send(&ClientMsg::End);
                    }
                }
            }
        }
    }
    std::thread::sleep(Duration::from_millis(700));
    let r = cowboy_core::ranch::load(proj.path(), "billing").unwrap();
    for wsid in ["schema", "api"] {
        if let Some(p) = r.workstream(wsid).and_then(|w| w.worktree_path.clone()) {
            let _ = Command::new("git")
                .arg("-C")
                .arg(proj.path())
                .args(["worktree", "remove", "--force"])
                .arg(&p)
                .output();
        }
    }
    reap_new_docker(&docker_before);
}

/// Ranch acceptance gate: a workstream that declares acceptance criteria does NOT
/// auto-complete when its session finishes — it pauses at `WaitingForUser`, the
/// dependent stays blocked, and the coordinator does not launch it. After the
/// user signs off (`ranch accept`), `ranch start` launches the dependent. Needs
/// Docker + a real model.
#[test]
#[ignore = "real Docker + model: exercises the acceptance gate + sign-off"]
fn e2e_ranch_acceptance_gate_pauses_until_signoff() {
    if !docker_ok() {
        eprintln!("skipping: docker not available");
        return;
    }
    let Some(_) = real_provider() else {
        eprintln!("skipping: no model provider in ~/.config/cowboy");
        return;
    };
    let docker_before = cowboy_docker_objects();
    let env = Env::real();
    let _d = env.spawn_daemon();
    let sock = env.sock();
    assert!(wait_pong(&sock));
    let proj = make_project();

    // schema declares acceptance criteria → it must pause for sign-off even after
    // its session completes; api depends on it.
    let ranch_dir = proj.path().join(".cowboy/ranches/billing");
    std::fs::create_dir_all(&ranch_dir).unwrap();
    std::fs::write(
        ranch_dir.join("ranch.yaml"),
        "version: 1\nid: billing\ntitle: Billing\nstatus: planning\nauto_advance: true\n\
         created_ms: 1\nupdated_ms: 1\nworkstreams:\n\
         \x20 - id: schema\n    title: Schema\n    goal: Create a file hello.txt containing the word hello, then finish.\n    depends_on: []\n    acceptance:\n      - a human confirms hello.txt is correct\n\
         \x20 - id: api\n    title: API\n    goal: Create a file api.txt containing the word api, then finish.\n    depends_on: [schema]\n",
    )
    .unwrap();

    let run_cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_cowboy"))
            .current_dir(proj.path())
            .env("XDG_RUNTIME_DIR", env.runtime.path())
            .env("XDG_STATE_HOME", env.state.path())
            .args(args)
            .output()
            .expect("run cowboy")
    };

    assert!(run_cli(&["ranch", "start", "billing"]).status.success());

    // Wait for schema's session to complete (it runs a trivial task).
    let schema_sid = cowboy_core::ranch::load(proj.path(), "billing")
        .unwrap()
        .workstream("schema")
        .and_then(|w| w.session_id.clone())
        .expect("schema should have a session");
    let mut completed = false;
    for _ in 0..1200 {
        if let Some(info) = get(&sock, &schema_sid) {
            if info.status == SessionStatus::Completed {
                completed = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(completed, "schema session should complete");

    // Give the coordinator a moment to run its advance, then assert it PAUSED:
    // schema is waiting for sign-off and api was NOT launched.
    std::thread::sleep(Duration::from_secs(2));
    let r = cowboy_core::ranch::load(proj.path(), "billing").unwrap();
    assert_eq!(
        r.workstream("schema").unwrap().status,
        cowboy_core::ranch::WorkstreamStatus::WaitingForUser,
        "schema should pause at the acceptance gate"
    );
    assert!(
        r.workstream("api").unwrap().session_id.is_none(),
        "api must not launch before sign-off"
    );

    // Sign off, then start: api now launches.
    assert!(run_cli(&["ranch", "accept", "billing", "schema"])
        .status
        .success());
    assert!(run_cli(&["ranch", "start", "billing"]).status.success());
    let mut api_launched = false;
    for _ in 0..50 {
        if cowboy_core::ranch::load(proj.path(), "billing")
            .unwrap()
            .workstream("api")
            .and_then(|w| w.session_id.clone())
            .is_some()
        {
            api_launched = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(api_launched, "api should launch after sign-off");

    // Cleanup: end any live workers, remove worktrees, reap containers.
    let r = cowboy_core::ranch::load(proj.path(), "billing").unwrap();
    for wsid in ["schema", "api"] {
        if let Some(w) = r.workstream(wsid) {
            if let Some(sid) = &w.session_id {
                if let Some(info) = get(&sock, sid) {
                    if let Some(ws) = &info.worker_sock {
                        Client::connect(ws).send(&ClientMsg::End);
                    }
                }
            }
        }
    }
    std::thread::sleep(Duration::from_millis(700));
    let r = cowboy_core::ranch::load(proj.path(), "billing").unwrap();
    for wsid in ["schema", "api"] {
        if let Some(p) = r.workstream(wsid).and_then(|w| w.worktree_path.clone()) {
            let _ = Command::new("git")
                .arg("-C")
                .arg(proj.path())
                .args(["worktree", "remove", "--force"])
                .arg(&p)
                .output();
        }
    }
    reap_new_docker(&docker_before);
}

/// Ranch scope proposals (agent path): a workstream worker, told to, uses the
/// `propose_scope_change` tool; the proposal lands PENDING in the main ranch's
/// proposals store (it must NOT edit ranch.yaml). Then `ranch approve` applies it.
/// Model-dependent (the agent must choose to call the tool) — exactly the kind of
/// behavior this manual suite is meant to check across models.
#[test]
#[ignore = "real Docker + model: exercises the propose_scope_change agent tool"]
fn e2e_ranch_agent_proposes_scope_change_then_user_approves() {
    if !docker_ok() {
        eprintln!("skipping: docker not available");
        return;
    }
    let Some(_) = real_provider() else {
        eprintln!("skipping: no model provider in ~/.config/cowboy");
        return;
    };
    let docker_before = cowboy_docker_objects();
    let env = Env::real();
    let _d = env.spawn_daemon();
    let sock = env.sock();
    assert!(wait_pong(&sock));
    let proj = make_project();

    // A single workstream whose task is to file a scope-change proposal.
    let ranch_dir = proj.path().join(".cowboy/ranches/billing");
    std::fs::create_dir_all(&ranch_dir).unwrap();
    std::fs::write(
        ranch_dir.join("ranch.yaml"),
        "version: 1\nid: billing\ntitle: Billing\nstatus: planning\nauto_advance: false\n\
         created_ms: 1\nupdated_ms: 1\nworkstreams:\n\
         \x20 - id: schema\n    title: Schema\n    goal: \"Call the propose_scope_change tool to propose adding a new workstream with workstream_id 'cache' (change=add_workstream), summary 'add a caching layer'. Then finish — do not do anything else.\"\n    depends_on: []\n",
    )
    .unwrap();

    let run_cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_cowboy"))
            .current_dir(proj.path())
            .env("XDG_RUNTIME_DIR", env.runtime.path())
            .env("XDG_STATE_HOME", env.state.path())
            .args(args)
            .output()
            .expect("run cowboy")
    };
    assert!(run_cli(&["ranch", "start", "billing"]).status.success());

    // Wait for a pending proposal to appear in the main ranch store.
    let mut proposal_id = None;
    for _ in 0..1500 {
        let pending = cowboy_core::scope::list(proj.path(), "billing");
        if let Some(p) = pending
            .into_iter()
            .find(|p| p.status == cowboy_core::scope::ProposalStatus::Pending)
        {
            proposal_id = Some(p);
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let p = proposal_id.expect("agent should file a pending scope proposal");
    assert!(
        matches!(
            p.change,
            cowboy_core::scope::ScopeChange::AddWorkstream { .. }
        ),
        "proposal should be an add_workstream: {:?}",
        p.change
    );
    // The agent must NOT have edited the plan itself.
    let r = cowboy_core::ranch::load(proj.path(), "billing").unwrap();
    assert!(
        r.workstream("cache").is_none(),
        "the plan must be unchanged until approval"
    );

    // The user approves → the plan now contains the new workstream.
    assert!(run_cli(&["ranch", "approve", "billing", &p.id])
        .status
        .success());
    let r = cowboy_core::ranch::load(proj.path(), "billing").unwrap();
    assert!(
        r.workstream("cache").is_some(),
        "approval should add the workstream"
    );

    // Cleanup: end the worker, remove its worktree, reap containers.
    let r = cowboy_core::ranch::load(proj.path(), "billing").unwrap();
    if let Some(w) = r.workstream("schema") {
        if let Some(sid) = &w.session_id {
            if let Some(info) = get(&sock, sid) {
                if let Some(ws) = &info.worker_sock {
                    Client::connect(ws).send(&ClientMsg::End);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        if let Some(p) = w.worktree_path.clone() {
            let _ = Command::new("git")
                .arg("-C")
                .arg(proj.path())
                .args(["worktree", "remove", "--force"])
                .arg(&p)
                .output();
        }
    }
    reap_new_docker(&docker_before);
}

/// Crew routing (agent path): with a crew roster configured, a planner that
/// delegates subagents has them routed through the roster — each launch logs a
/// `SubagentRouted` lifecycle event with the resolved model. Uses an isolated
/// config home (copies the real provider/models, writes its own crew.yaml) so it
/// never touches `~/.config/cowboy`. Model-dependent + needs Docker.
#[test]
#[ignore = "real Docker + model: exercises crew routing of subagents"]
fn e2e_crew_routes_delegated_subagents() {
    if !docker_ok() {
        eprintln!("skipping: docker not available");
        return;
    }
    let Some(real_providers) = real_provider() else {
        eprintln!("skipping: no model provider in ~/.config/cowboy");
        return;
    };
    let real_dir = real_providers.parent().unwrap().to_path_buf();
    let docker_before = cowboy_docker_objects();

    // Isolated config home: copy the real provider + models, add our own crew.yaml.
    let cfg = assert_fs::TempDir::new().unwrap();
    let cfg_cowboy = cfg.path().join("cowboy");
    std::fs::create_dir_all(&cfg_cowboy).unwrap();
    std::fs::copy(&real_providers, cfg_cowboy.join("providers.yaml")).unwrap();
    std::fs::copy(real_dir.join("models.yaml"), cfg_cowboy.join("models.yaml")).unwrap();
    let models = cowboy_core::config::ModelsConfig::load(&cfg_cowboy.join("models.yaml")).unwrap();
    let default_model = models.default.clone().expect("a default model");
    // Route everything at the one real model so the subagents actually run.
    std::fs::write(
        cfg_cowboy.join("crew.yaml"),
        format!(
            "version: 1\nplanner:\n  model: {default_model}\ncrew:\n  general: {default_model}\n  \
             tests: {default_model}\ndelegation:\n  max_parallel: 4\n  max_depth: 1\n"
        ),
    )
    .unwrap();

    let runtime = assert_fs::TempDir::new().unwrap();
    let state = assert_fs::TempDir::new().unwrap();
    let proj = make_project();

    let out = Command::new(env!("CARGO_BIN_EXE_cowboy"))
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", cfg.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_STATE_HOME", state.path())
        .stdin(std::process::Stdio::null())
        .arg(
            "Delegate two subagents with the `subagent` tool: one with category=tests effort=small \
             task 'create a file a.txt containing the letter a', another with category=general \
             effort=small task 'create a file b.txt containing the letter b'. Then finish.",
        )
        .output()
        .expect("run cowboy one-shot");
    assert!(
        out.status.success(),
        "one-shot failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Scan the session lifecycle logs for routing events.
    let mut routed = Vec::new();
    let sessions = proj.path().join(".cowboy/sessions");
    if let Ok(entries) = std::fs::read_dir(&sessions) {
        for e in entries.flatten() {
            let lc = e.path().join("lifecycle.jsonl");
            if let Ok(text) = std::fs::read_to_string(&lc) {
                for line in text.lines() {
                    if let Ok(rec) =
                        serde_json::from_str::<cowboy_core::lifecycle::LifecycleRecord>(line)
                    {
                        if let cowboy_core::lifecycle::LifecycleEvent::SubagentRouted {
                            model,
                            ..
                        } = rec.event
                        {
                            routed.push(model);
                        }
                    }
                }
            }
        }
    }
    assert!(
        !routed.is_empty(),
        "expected at least one SubagentRouted event; the planner didn't delegate"
    );
    assert!(
        routed.iter().all(|m| m == &default_model),
        "subagents should route to the rostered model {default_model}, got {routed:?}"
    );

    reap_new_docker(&docker_before);
}

/// The flagship real-Docker turn: the daemon starts a session that runs an
/// actual agent turn against the configured model, a client streams it, detach
/// leaves it running, and re-attach replays the journal.
#[test]
#[ignore = "real Docker + model: needs docker, cowboy/agent:local, and ~/.config/cowboy"]
fn e2e_turn_streams_detach_keeps_running_then_reattach_replays() {
    if !docker_ok() {
        eprintln!("skipping: docker not available");
        return;
    }
    let Some(_) = real_provider() else {
        eprintln!("skipping: no model provider in ~/.config/cowboy");
        return;
    };

    let env = Env::real();
    let _d = env.spawn_daemon();
    let sock = env.sock();
    assert!(wait_pong(&sock));
    let proj = make_project();

    let (id, ws) = match start(
        &sock,
        proj.path(),
        Some("Create a file e2e.txt containing exactly: ok. Then you are done."),
    ) {
        DaemonResp::Started { id, worker_sock } => (id, worker_sock),
        other => panic!("expected Started, got {other:?}"),
    };

    // Attach and drive the turn to completion.
    let mut a = Client::connect(&ws);
    a.hello(None);
    let mut saw_final = false;
    let mut saw_tool = false;
    loop {
        match a.recv() {
            Some(ServerMsg::Event { event, .. }) => match event {
                UiEventMsg::ToolUse(_) => saw_tool = true,
                UiEventMsg::Final(_) => saw_final = true,
                UiEventMsg::TurnDone => break,
                _ => {}
            },
            Some(ServerMsg::Ended { .. }) | None => break,
            _ => {}
        }
    }
    assert!(saw_tool, "the agent should have used a tool");
    assert!(saw_final, "the turn should produce a final message");
    assert_eq!(
        std::fs::read_to_string(proj.path().join("e2e.txt"))
            .unwrap_or_default()
            .trim(),
        "ok",
        "the agent should have created e2e.txt"
    );

    // Detach (not End): the session must stay alive and non-terminal.
    a.send(&ClientMsg::Detach);
    drop(a);
    std::thread::sleep(Duration::from_millis(500));
    let status = get(&sock, &id).map(|s| s.status);
    assert!(
        matches!(status, Some(s) if !s.is_terminal()),
        "detached session should still be running, was {status:?}"
    );

    // Re-attach from the start: the journal replays (we see the Final again).
    let mut b = Client::connect(&ws);
    b.hello(Some(0));
    let mut journal_len = 0;
    let mut replayed_final = false;
    loop {
        match b.recv() {
            Some(ServerMsg::Snapshot { journal_len: n, .. }) => journal_len = n,
            Some(ServerMsg::Event {
                event: UiEventMsg::Final(_),
                ..
            }) => {
                replayed_final = true;
                break;
            }
            Some(ServerMsg::Event { seq, .. }) if seq + 1 >= journal_len => break,
            Some(_) => {}
            None => break,
        }
    }
    assert!(
        journal_len > 0,
        "re-attach snapshot should report a journal"
    );
    assert!(replayed_final, "re-attach should replay the final message");

    // Clean shutdown + remove the container/network we created.
    b.send(&ClientMsg::End);
    std::thread::sleep(Duration::from_millis(800));
    let _ = Command::new(env!("CARGO_BIN_EXE_cowboy"))
        .current_dir(proj.path())
        .arg("down")
        .output();
}
