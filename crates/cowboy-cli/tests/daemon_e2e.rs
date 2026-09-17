//! End-to-end tests for the `cowboyd` daemon + worker + client stack, exercised
//! through the real `cowboy`/`cowboyd` binaries and unix sockets.
//!
//! All `#[ignore]`: they spawn real worker processes (and, for the turn test, run
//! real commands in a real sandbox against a configured model). Run them explicitly:
//!
//! ```text
//! cargo test -p cowboy-cli --test daemon_e2e -- --ignored
//! ```
//!
//! Each test self-skips (prints why, returns Ok) when its prerequisites are absent,
//! so `--ignored` is safe to run anywhere. The tests that execute a turn need a
//! working sandbox (kernel prerequisites — see `cowboy doctor`) and a model provider
//! in `~/.config/cowboy`; the rest only need a model *provider* to exist (the worker
//! resolves one at startup but, with no task, never calls it), so they supply a fake.
//!
//! Cleanup is much smaller than it was. A session's namespaces, interception ruleset
//! and cgroup all belong to a holder process whose lifetime is tied to its worker's,
//! so ending the worker releases them; only an empty cgroup *directory* can linger.

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
use cowboy_core::netproto::encode_line;

// ---------------------------------------------------------------------------
// Prerequisites / skip helpers
// ---------------------------------------------------------------------------

/// Whether this host can actually run a sandbox, so a test that needs a command to
/// execute skips cleanly rather than failing for the wrong reason.
///
/// Checked by creating a user namespace, which is the prerequisite everything else
/// rests on. `cowboy doctor` reports the full list.
fn sandbox_ok() -> bool {
    Command::new("bwrap")
        .args([
            "--unshare-user",
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            "usr/lib64",
            "/lib64",
            "--",
            "/usr/bin/true",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Reap anything a finished session could have left behind.
///
/// Only empty cgroup directories can outlive a worker, and only because the kernel
/// keeps a cgroup until someone removes the directory. There are no containers,
/// networks, or ruleset remnants to chase: they lived in namespaces the worker's exit
/// released.
fn reap_session_residue() {
    let reaped = cowboy_cli::sandbox::cgroup::reap_empty();
    if reaped > 0 {
        eprintln!("reaped {reaped} leftover cgroup(s)");
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

    /// A **real** provider with a **controlled crew roster**: the user's
    /// `providers.yaml`/`models.yaml` are copied into a throwaway config home and a
    /// `crew.yaml` is written next to them.
    ///
    /// Delegation is gated on a roster existing, and the turn grants under test come
    /// from it, so a delegation test cannot depend on whatever the developer happens to
    /// have configured. Returns `None` when there is no real provider to copy.
    fn real_with_crew(crew_yaml: &str) -> Option<Self> {
        let providers = real_provider()?;
        let src = providers.parent()?.to_path_buf();
        let config = assert_fs::TempDir::new().unwrap();
        let dst = config.child("cowboy");
        dst.create_dir_all().unwrap();
        for name in ["providers.yaml", "models.yaml"] {
            let from = src.join(name);
            if from.is_file() {
                std::fs::copy(&from, dst.path().join(name)).unwrap();
            }
        }
        std::fs::write(dst.path().join("crew.yaml"), crew_yaml).unwrap();
        Some(Self {
            runtime: assert_fs::TempDir::new().unwrap(),
            state: assert_fs::TempDir::new().unwrap(),
            config: Some(config),
        })
    }

    fn sock(&self) -> PathBuf {
        self.runtime.path().join("cowboy/cowboyd.sock")
    }

    fn spawn_daemon(&self) -> Kill {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cowboyd"));
        cmd.env("XDG_RUNTIME_DIR", self.runtime.path())
            .env("XDG_STATE_HOME", self.state.path())
            // Pin the gateway control token so a faked gateway can authenticate
            // over the TCP control channel (workers inherit the daemon's env).
            .env("COWBOY_CONTROL_TOKEN", E2E_CONTROL_TOKEN);
        if let Some(c) = &self.config {
            cmd.env("XDG_CONFIG_HOME", c.path());
        }
        Kill(cmd.spawn().expect("spawn cowboyd"))
    }
}

/// Known control token the e2e daemon pins (see `spawn_daemon`) so a test can
/// connect to the control channel as a faked gateway.
const E2E_CONTROL_TOKEN: &str = "e2e-control-token";

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
    // Don't inherit a developer's global commit.gpgsign (test identity has no key).
    git(&["config", "commit.gpgsign", "false"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    dir
}

/// Select a model for live workstream workers and commit it. Ranch workstreams run
/// in worktrees checked out from HEAD, so the model choice must be committed for the
/// worker (rooted at the worktree) to resolve it — the home default may not be valid
/// in every environment. Override the model with `COWBOY_E2E_MODEL`.
fn commit_model(dir: &Path, env: &Env) {
    let model = std::env::var("COWBOY_E2E_MODEL").unwrap_or_else(|_| "cerebras/zai-glm-4.7".into());
    let ok = Command::new(env!("CARGO_BIN_EXE_cowboy"))
        .current_dir(dir)
        .env("XDG_RUNTIME_DIR", env.runtime.path())
        .env("XDG_STATE_HOME", env.state.path())
        .args(["models", "use", &model])
        .output()
        .expect("run models use")
        .status
        .success();
    assert!(ok, "models use {model} should succeed");
    for args in [&["add", "-A"][..], &["commit", "-qm", "model"][..]] {
        let _ = Command::new("git").arg("-C").arg(dir).args(args).output();
    }
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

/// A live `SessionInfo` for a fake lease holder (our own pid, so it reads as
/// alive and its exclusive lease is NOT reclaimable — like a real parent).
fn fake_parent(id: &str, root: &Path) -> cowboy_core::daemonproto::SessionInfo {
    cowboy_core::daemonproto::SessionInfo {
        id: id.into(),
        root: root.to_path_buf(),
        task: None,
        status: SessionStatus::Running,
        pid: Some(std::process::id()),
        branch: None,
        session_name: None,
        worker_sock: None,
        journal_path: None,
        lease_mode: Some(LeaseMode::Exclusive),
        started_ms: 1,
        last_heartbeat_ms: 1,
        turn: 0,
        tokens: (0, 0),
        attached_clients: 0,
        diffstat: String::new(),
        running_command: None,
        blocked_reason: None,
        ranch_id: None,
        workstream_id: None,
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

    /// Collect display events until `stop` says so (or the stream ends / the read
    /// timeout fires). Returns every `UiEventMsg` seen, so an assertion can look at the
    /// whole turn rather than racing a single message.
    fn drain_until(&mut self, mut stop: impl FnMut(&UiEventMsg) -> bool) -> Vec<UiEventMsg> {
        let mut seen = Vec::new();
        loop {
            match self.recv() {
                Some(ServerMsg::Event { event, .. }) => {
                    let done = stop(&event);
                    seen.push(event);
                    if done {
                        return seen;
                    }
                }
                Some(ServerMsg::Ended { .. }) | None => return seen,
                Some(_) => {}
            }
        }
    }
}

/// A short name for a display event, so a failing e2e prints a readable timeline
/// instead of pages of pretty-printed payloads.
fn event_name(e: &UiEventMsg) -> String {
    match e {
        UiEventMsg::SubagentPending { id, .. } => format!("SubagentPending({id})"),
        UiEventMsg::SubagentStarted { id, .. } => format!("SubagentStarted({id})"),
        UiEventMsg::SubagentDone { id, ok, .. } => format!("SubagentDone({id},ok={ok})"),
        UiEventMsg::JobsChanged(j) => format!(
            "JobsChanged[{}]",
            j.iter()
                .map(|x| format!("{}:{}", x.id, x.state))
                .collect::<Vec<_>>()
                .join(",")
        ),
        UiEventMsg::CommandStart(c) => format!("CommandStart({c})"),
        UiEventMsg::ToolUse(s) => format!("ToolUse({s})"),
        UiEventMsg::Notice(n) => format!("Notice({n})"),
        UiEventMsg::SteerDelivered(t) => format!("Steer({t})"),
        UiEventMsg::Final(_) => "Final".into(),
        UiEventMsg::TurnDone => "TurnDone".into(),
        UiEventMsg::Delta(_) => "Delta".into(),
        UiEventMsg::Reasoning(_) => "Reasoning".into(),
        UiEventMsg::CommandOutput(_) => "CommandOutput".into(),
        UiEventMsg::CommandEnd { code, .. } => format!("CommandEnd({code})"),
        UiEventMsg::UserMessage(_) => "UserMessage".into(),
        UiEventMsg::FileDiff { path, .. } => format!("FileDiff({path})"),
        UiEventMsg::QueueChanged { pending } => format!("QueueChanged({})", pending.len()),
        _ => "…".into(),
    }
}

/// A crew roster with one cheap model for everything and a **tiny** turn grant, so a
/// delegated worker runs out quickly and has to ask for more.
///
/// `model` must exist in the copied `models.yaml`; `<default>` means "the foreman's
/// model", which is the only name guaranteed to resolve on any developer's machine.
fn crew_yaml(grant: u32, ceiling: u32) -> String {
    format!(
        "version: 1\ncrew:\n  general: \"<default>\"\ndelegation:\n  enabled: true\n  \
         max_parallel: 4\n  max_parallel_per_provider: 2\n  max_depth: 1\n  \
         iterations:\n    tiny: {grant}\n  max_total_iterations: {ceiling}\n  \
         request_timeout_seconds: 60\n"
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Async delegation, end to end: the foreman dispatches a subagent and **keeps
/// working in the same turn** instead of blocking on it.
///
/// This is the defect the whole change exists to fix, and it can only be checked
/// against a real model: the ordering that matters is "dispatch, then the foreman does
/// something else, then the result arrives". Asserted on the journal's event order, not
/// on wording.
#[test]
#[ignore = "real model: dispatches a real subagent process"]
fn e2e_delegation_does_not_block_the_foreman() {
    let Some(env) = Env::real_with_crew(&crew_yaml(25, 400)) else {
        eprintln!("skipping: no model provider in ~/.config/cowboy");
        return;
    };
    if !sandbox_ok() {
        eprintln!("skipping: this host cannot create a user namespace");
        return;
    }
    let _d = env.spawn_daemon();
    let sock = env.sock();
    assert!(wait_pong(&sock));
    let proj = make_project();

    let task = "Use the `subagent` tool ONCE to delegate this task: \"write the single \
        line 'hello from the subagent' to the file sub.txt, then finish\" (category \
        general, effort tiny). The tool returns a job id, NOT the answer. Immediately \
        after dispatching, and BEFORE the job finishes, use the `write` tool to create \
        foreman.txt containing the word DISPATCHED. Then use `wait`. When the \
        subagent's result arrives, call `final` with a one-line summary.";
    let (id, ws) = match start(&sock, proj.path(), Some(task)) {
        DaemonResp::Started { id, worker_sock } => (id, worker_sock),
        other => panic!("expected Started, got {other:?}"),
    };

    let mut a = Client::connect(&ws);
    a.hello(None);
    let events = a.drain_until(|e| matches!(e, UiEventMsg::TurnDone));
    a.send(&ClientMsg::End);
    std::thread::sleep(Duration::from_millis(800));
    let _ = Command::new(env!("CARGO_BIN_EXE_cowboy"))
        .current_dir(proj.path())
        .arg("down")
        .output();
    reap_session_residue();

    // The foreman's own write must land *before* the delegated result arrives —
    // proof it was not parked waiting.
    let pos = |pred: fn(&UiEventMsg) -> bool| events.iter().position(pred);
    let dispatched = pos(|e| matches!(e, UiEventMsg::SubagentPending { .. }))
        .or_else(|| pos(|e| matches!(e, UiEventMsg::SubagentStarted { .. })))
        .unwrap_or_else(|| panic!("no subagent was dispatched; events: {events:#?}"));
    let own_work = events
        .iter()
        .position(|e| match e {
            UiEventMsg::ToolUse(s) => s.contains("foreman.txt"),
            UiEventMsg::FileDiff { path, .. } => path.contains("foreman.txt"),
            _ => false,
        })
        .unwrap_or_else(|| panic!("the foreman did no work of its own; events: {events:#?}"));
    let result_arrived = events
        .iter()
        .position(|e| matches!(e, UiEventMsg::SubagentDone { .. }))
        .unwrap_or_else(|| panic!("the subagent never finished; events: {events:#?}"));
    assert!(
        dispatched < own_work && own_work < result_arrived,
        "the foreman must work between dispatch and delivery (dispatch={dispatched}, \
         own work={own_work}, result={result_arrived})"
    );
    // And both wrote their files, so the subagent really ran in the shared workspace.
    assert!(
        proj.path().join("foreman.txt").is_file(),
        "the foreman's own file should exist"
    );
    assert!(
        proj.path().join("sub.txt").is_file(),
        "the subagent's file should exist"
    );
    // The job's control directory is cleaned up when it finishes.
    let jobs_dir = env.state.path().join("cowboy/jobs").join(&id);
    assert!(
        !jobs_dir.exists() || std::fs::read_dir(&jobs_dir).map(|d| d.count()).unwrap_or(0) == 0,
        "the job control dir should be cleaned up: {}",
        jobs_dir.display()
    );
}

/// Grant-and-request against a real model: a worker given a tiny grant runs out,
/// reports, and the **foreman** decides. Asserted from the control channel + the
/// worker's own final answer, so both halves of the mechanism are covered.
#[test]
#[ignore = "real model: exercises the turn-request loop"]
fn e2e_a_worker_that_runs_out_of_turns_reports_and_is_answered() {
    // A one-turn grant guarantees the request happens on the first boundary.
    let Some(env) = Env::real_with_crew(&crew_yaml(1, 40)) else {
        eprintln!("skipping: no model provider in ~/.config/cowboy");
        return;
    };
    if !sandbox_ok() {
        eprintln!("skipping: this host cannot create a user namespace");
        return;
    }
    let _d = env.spawn_daemon();
    let sock = env.sock();
    assert!(wait_pong(&sock));
    let proj = make_project();

    let task = "Use the `subagent` tool ONCE (category general, effort tiny) to \
        delegate: \"list every file in the repository, then write a one-paragraph \
        summary of the project to summary.md, then finish\". Its turn grant is \
        deliberately tiny, so it will report progress and ask you for more turns: when \
        it does, answer with `job_reply` verdict=grant, iterations=15. Then `wait` for \
        its result and call `final` with a one-line summary.";
    let (id, ws) = match start(&sock, proj.path(), Some(task)) {
        DaemonResp::Started { id, worker_sock } => (id, worker_sock),
        other => panic!("expected Started, got {other:?}"),
    };

    let mut a = Client::connect(&ws);
    a.hello(None);
    let events = a.drain_until(|e| matches!(e, UiEventMsg::TurnDone));
    a.send(&ClientMsg::End);
    std::thread::sleep(Duration::from_millis(800));
    let _ = Command::new(env!("CARGO_BIN_EXE_cowboy"))
        .current_dir(proj.path())
        .arg("down")
        .output();
    reap_session_residue();

    // The foreman was told about the request (the notice names the ask).
    let notices: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            UiEventMsg::Notice(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        notices.iter().any(|n| n.contains("asking for")),
        "the foreman should be told a worker wants more turns; notices: {notices:#?}"
    );
    // Whatever the foreman decided, the worker must have ended with a REPORT rather
    // than silently hitting a cap: that is the outcome this replaced.
    let sub_id = events
        .iter()
        .find_map(|e| match e {
            UiEventMsg::SubagentStarted { id, .. } | UiEventMsg::SubagentPending { id, .. } => {
                Some(id.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no subagent was dispatched; events: {events:#?}"));
    let sub_final = std::fs::read_to_string(
        proj.path()
            .join(".cowboy/sessions")
            .join(&sub_id)
            .join("final.md"),
    )
    .unwrap_or_default();
    assert!(
        !sub_final.trim().is_empty(),
        "the worker must finish with a written answer, not a silent cap; \
         session {sub_id} produced no final.md"
    );
    assert!(
        !sub_final.contains("[partial]"),
        "grant-and-request should replace the `[partial]` outcome; got:\n{sub_final}"
    );
    let _ = id;
}

/// Mid-turn steering against a real model: a message typed while the agent works
/// reaches it *within the same turn*, and is acted on.
#[test]
#[ignore = "real model: steers a running turn"]
fn e2e_steering_reaches_a_running_turn() {
    let Some(_) = real_provider() else {
        eprintln!("skipping: no model provider in ~/.config/cowboy");
        return;
    };
    if !sandbox_ok() {
        eprintln!("skipping: this host cannot create a user namespace");
        return;
    }
    let env = Env::real();
    let _d = env.spawn_daemon();
    let sock = env.sock();
    assert!(wait_pong(&sock));
    let proj = make_project();

    // A deliberately slow first step, so the steer lands while the turn is running.
    let task = "Run the shell command `sleep 6` first. Then write the file first.txt \
        containing FIRST. Then call `final`.";
    let (_id, ws) = match start(&sock, proj.path(), Some(task)) {
        DaemonResp::Started { id, worker_sock } => (id, worker_sock),
        other => panic!("expected Started, got {other:?}"),
    };

    let mut a = Client::connect(&ws);
    a.hello(None);
    // Wait until the turn is demonstrably underway, then speak.
    a.drain_until(|e| matches!(e, UiEventMsg::CommandStart(c) if c.contains("sleep")));
    // Phrased as an instruction the model cannot read as optional. The delivery half of
    // this test is deterministic (`SteerDelivered` below), but "did the model then act on
    // it" depends on the model choosing to, and a politely-worded aside got skipped in
    // favour of the plan it had already made — failing the test for a reason that is not
    // cowboy's behaviour.
    a.send(&ClientMsg::Message(
        "IMPORTANT, do this before you call final: also write the file steered.txt \
         containing STEERED."
            .into(),
    ));
    let events = a.drain_until(|e| matches!(e, UiEventMsg::TurnDone));
    a.send(&ClientMsg::End);
    std::thread::sleep(Duration::from_millis(800));
    let _ = Command::new(env!("CARGO_BIN_EXE_cowboy"))
        .current_dir(proj.path())
        .arg("down")
        .output();
    reap_session_residue();

    assert!(
        events
            .iter()
            .any(|e| matches!(e, UiEventMsg::SteerDelivered(t) if t.contains("steered.txt"))),
        "the steer should be delivered into the running turn; events: {events:#?}"
    );
    assert!(
        proj.path().join("steered.txt").is_file(),
        "the agent should act on mid-turn input within the same turn"
    );
    assert!(
        proj.path().join("first.txt").is_file(),
        "steering must not cancel the work already in progress"
    );
}

/// Interrupting a turn leaves the delegated work alone: the subagent keeps running and
/// its result arrives in a **later** turn. Under the old batch-join this work was
/// killed and re-done.
#[test]
#[ignore = "real model: interrupts a turn with a subagent in flight"]
fn e2e_an_interrupt_keeps_the_subagents_and_their_results() {
    let Some(env) = Env::real_with_crew(&crew_yaml(25, 400)) else {
        eprintln!("skipping: no model provider in ~/.config/cowboy");
        return;
    };
    if !sandbox_ok() {
        eprintln!("skipping: this host cannot create a user namespace");
        return;
    }
    let _d = env.spawn_daemon();
    let sock = env.sock();
    assert!(wait_pong(&sock));
    let proj = make_project();

    let task = "Use the `subagent` tool ONCE (category general, effort tiny) to \
        delegate: \"run the shell command `sleep 5`, then write the line DONE to \
        worker.txt, then finish\". After dispatching, call `wait`.";
    let (_id, ws) = match start(&sock, proj.path(), Some(task)) {
        DaemonResp::Started { id, worker_sock } => (id, worker_sock),
        other => panic!("expected Started, got {other:?}"),
    };

    let mut a = Client::connect(&ws);
    a.hello(None);
    // Once the worker is actually running, interrupt the *foreman's* turn.
    a.drain_until(|e| matches!(e, UiEventMsg::SubagentStarted { .. }));
    a.send(&ClientMsg::Interrupt {
        kind: cowboy_core::daemonproto::InterruptKind::Turn,
    });
    // Everything from the interrupt onwards, across both turns: the result is
    // delivered at whichever iteration boundary comes first, and which turn that falls
    // in is a timing detail, not the property under test.
    let mut events = a.drain_until(|e| matches!(e, UiEventMsg::TurnDone));

    // A second turn: the pre-interrupt worker's result must reach the conversation.
    a.send(&ClientMsg::Message(
        "What did the subagent you dispatched earlier report? Answer from what you \
         already know and call `final` — do not run any commands."
            .into(),
    ));
    events.extend(a.drain_until(|e| matches!(e, UiEventMsg::TurnDone)));
    a.send(&ClientMsg::End);
    std::thread::sleep(Duration::from_millis(800));
    let _ = Command::new(env!("CARGO_BIN_EXE_cowboy"))
        .current_dir(proj.path())
        .arg("down")
        .output();
    reap_session_residue();

    // The interrupt did not reap the worker: it finished its `sleep` and wrote its file.
    assert!(
        proj.path().join("worker.txt").is_file(),
        "the subagent should have kept running through the interrupt"
    );
    // And its result reached the foreman rather than being lost with the cancelled
    // turn: it can only report the worker's outcome if the result was delivered.
    //
    // Asserted on the answer rather than on a `SubagentDone` edge: *which* turn's event
    // stream carries the delivery depends on when the child happens to exit relative to
    // the interrupt unwind, and that timing is not the property under test. The
    // deterministic version of the event-level claim is
    // `a_result_from_before_an_interrupt_arrives_in_the_next_turn` in the unit tests.
    let last_final = events
        .iter()
        .rev()
        .find_map(|e| match e {
            UiEventMsg::Final(m) => Some(m.clone()),
            _ => None,
        })
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        last_final.contains("worker.txt") || last_final.contains("done"),
        "the foreman should be able to report the subagent's result after the \
         interrupt; got final: {last_final:?}\nevents: {:#?}",
        events
            .iter()
            .map(event_name)
            .collect::<Vec<_>>()
            .join(" → ")
    );
}

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

    // The surviving worker must still be reachable on its original socket — a
    // restarted daemon that lost its state must not prune a live worker's socket
    // before it re-heartbeats (regression: startup prune_sockets nuked it).
    assert!(
        ws.exists(),
        "surviving worker's socket should outlive a daemon restart"
    );

    Client::connect(&ws).send(&ClientMsg::End);
    std::thread::sleep(Duration::from_millis(500));
}

/// A `Shutdown` request exits the daemon gracefully **without** killing its
/// workers — the mechanism an upgraded CLI uses to roll a stale-version daemon.
/// The worker survives and a successor daemon re-adopts it.
#[test]
#[ignore = "spawns real worker processes"]
fn e2e_shutdown_request_exits_daemon_and_keeps_workers() {
    let env = Env::fake();
    let sock = env.sock();
    let proj = make_project();

    let d1 = env.spawn_daemon();
    assert!(wait_pong(&sock));
    let (id, ws) = match start(&sock, proj.path(), None) {
        DaemonResp::Started { id, worker_sock } => (id, worker_sock),
        other => panic!("expected Started, got {other:?}"),
    };

    // Graceful shutdown: the daemon acks and then exits on its own.
    assert!(matches!(
        dreq(&sock, DaemonReq::Shutdown),
        Some(DaemonResp::ShuttingDown)
    ));
    let mut down = false;
    for _ in 0..50 {
        if !matches!(dreq(&sock, DaemonReq::Ping), Some(DaemonResp::Pong { .. })) {
            down = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(down, "daemon should stop serving after Shutdown");
    // The worker was left running — its socket is still bound.
    assert!(ws.exists(), "Shutdown must not kill workers");
    drop(d1); // already exited; Kill-drop is a no-op kill

    // A successor daemon re-adopts the surviving worker via its heartbeat.
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
        "successor daemon should re-adopt the surviving worker"
    );

    Client::connect(&ws).send(&ClientMsg::End);
    std::thread::sleep(Duration::from_millis(500));
}

/// Foundations against a real model: a single turn must drive the coordination
/// tools (artifact, blocked/unblock, handoff) and leave the right on-disk
/// effects — the regression check for prompt/model compatibility. Asserts the
/// file effects (robust to wording), not the transcript. Needs a model provider
/// but not the sandbox (these tools run host-side; the task does no shell/network).
#[test]
#[ignore = "real model: needs a provider in ~/.config/cowboy"]
fn e2e_foundation_tools_record_artifacts_lifecycle_handoff() {
    let Some(_) = real_provider() else {
        eprintln!("skipping: no model provider in ~/.config/cowboy");
        return;
    };

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
    reap_session_residue();

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
    reap_session_residue();
}

/// Ranch workstream lifecycle (interactive model): start a ranch's first
/// workstream, let it run its initial attempt, and assert it IDLES
/// (stays `Running`, never auto-completes) and the dependent does NOT auto-launch.
/// Then sign off the way the TUI's `/accept` does — send `ClientMsg::Accept` to
/// the worker — and assert the full loop: worker → `AcceptWorkstream` → daemon
/// completes schema + advances → api auto-launches, and the schema session ends.
/// Needs a working sandbox + a real model.
#[test]
#[ignore = "real sandbox + model: exercises idle workstream + in-session /accept"]
fn e2e_ranch_workstream_idles_then_signoff_advances() {
    if !sandbox_ok() {
        eprintln!("skipping: the sandbox cannot run here (see `cowboy doctor`)");
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

    // schema does a trivial first attempt; api depends on it. A workstream is
    // never auto-completed — it idles after its first attempt until the user
    // signs off, whatever its acceptance criteria.
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
    commit_model(proj.path(), &env);

    // Launch only the ready workstream (schema).
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

    // Wait for schema's first attempt to finish (one turn done) — it then idles.
    let schema_sid = cowboy_core::ranch::load(proj.path(), "billing")
        .unwrap()
        .workstream("schema")
        .and_then(|w| w.session_id.clone())
        .expect("schema should have a session");
    let mut first_attempt_done = false;
    for _ in 0..1200 {
        if let Some(info) = get(&sock, &schema_sid) {
            // First turn complete + still Running = idling, not auto-completed.
            if info.turn >= 1 && info.status == SessionStatus::Running {
                first_attempt_done = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        first_attempt_done,
        "schema should finish a first attempt and idle (Running)"
    );

    // It must NOT have auto-completed, and api must NOT have auto-launched.
    let info = get(&sock, &schema_sid).expect("schema session");
    assert_eq!(
        info.status,
        SessionStatus::Running,
        "workstream must idle, not auto-complete"
    );
    let r = cowboy_core::ranch::load(proj.path(), "billing").unwrap();
    assert!(
        r.workstream("api").unwrap().session_id.is_none(),
        "api must not auto-launch before sign-off"
    );

    // Sign off exactly as the TUI's `/accept` does: send Accept to the worker.
    let schema_ws_sock = info.worker_sock.clone().expect("schema worker sock");
    Client::connect(&schema_ws_sock).send(&ClientMsg::Accept { note: None });

    // The worker asks the daemon to complete schema + advance the plan, then ends.
    // Wait for api to auto-launch — proof the in-session sign-off advanced the plan.
    let mut api = None;
    for _ in 0..600 {
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
    let api = api.expect("api should launch after in-session sign-off");
    assert_eq!(api.ranch_id.as_deref(), Some("billing"));

    let r = cowboy_core::ranch::load(proj.path(), "billing").unwrap();
    assert!(
        r.workstream("schema").unwrap().status.is_done(),
        "schema should be complete after sign-off: {:?}",
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
    reap_session_residue();
}

/// Ranch sign-off via the CLI fallback (`cowboy ranch accept`): the same as the
/// in-session path, but signing off from the shell while the idle worker is not
/// attached. After `ranch accept`, the dependent launches on the next advance.
/// Needs a working sandbox + a real model.
#[test]
#[ignore = "real sandbox + model: exercises `cowboy ranch accept` sign-off"]
fn e2e_ranch_cli_accept_signoff_advances() {
    if !sandbox_ok() {
        eprintln!("skipping: the sandbox cannot run here (see `cowboy doctor`)");
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
    commit_model(proj.path(), &env);

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

    // Wait for schema's first attempt to finish; it then idles (Running).
    let schema_sid = cowboy_core::ranch::load(proj.path(), "billing")
        .unwrap()
        .workstream("schema")
        .and_then(|w| w.session_id.clone())
        .expect("schema should have a session");
    let mut idling = false;
    for _ in 0..1200 {
        if let Some(info) = get(&sock, &schema_sid) {
            if info.turn >= 1 && info.status == SessionStatus::Running {
                idling = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(idling, "schema should finish a first attempt and idle");

    // api must not have auto-launched.
    assert!(
        cowboy_core::ranch::load(proj.path(), "billing")
            .unwrap()
            .workstream("api")
            .unwrap()
            .session_id
            .is_none(),
        "api must not auto-launch before sign-off"
    );

    // Sign off from the shell, then advance: api launches.
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
    assert!(api_launched, "api should launch after CLI sign-off");

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
    reap_session_residue();
}

/// Ranch scope proposals (agent path): a workstream worker, told to, uses the
/// `propose_scope_change` tool; the proposal lands PENDING in the main ranch's
/// proposals store (it must NOT edit ranch.yaml). Then `ranch approve` applies it.
/// Model-dependent (the agent must choose to call the tool) — exactly the kind of
/// behavior this manual suite is meant to check across models.
#[test]
#[ignore = "real sandbox + model: exercises the propose_scope_change agent tool"]
fn e2e_ranch_agent_proposes_scope_change_then_user_approves() {
    if !sandbox_ok() {
        eprintln!("skipping: the sandbox cannot run here (see `cowboy doctor`)");
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
    reap_session_residue();
}

/// Crew routing (agent path): with a crew roster configured, a planner that
/// delegates subagents has them routed through the roster — each launch logs a
/// `SubagentRouted` lifecycle event with the resolved model. Uses an isolated
/// config home (copies the real provider/models, writes its own crew.yaml) so it
/// never touches `~/.config/cowboy`. Model-dependent; needs a working sandbox.
#[test]
#[ignore = "real sandbox + model: exercises crew routing of subagents"]
fn e2e_crew_routes_delegated_subagents() {
    if !sandbox_ok() {
        eprintln!("skipping: the sandbox cannot run here (see `cowboy doctor`)");
        return;
    }
    let Some(real_providers) = real_provider() else {
        eprintln!("skipping: no model provider in ~/.config/cowboy");
        return;
    };
    let real_dir = real_providers.parent().unwrap().to_path_buf();

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

    reap_session_residue();
}

/// Subagents share their parent's worktree by design — a subagent (run with a
/// held worktree lease) must NOT try to acquire the lease, or it's denied and
/// returns nothing ("no final answer"). This reproduces a parent holding the
/// exclusive lease and asserts a subagent-style child still produces a final
/// answer. Needs a working sandbox + a real model.
#[test]
#[ignore = "real sandbox + model: subagent runs in a parent-held worktree"]
fn e2e_subagent_runs_in_held_worktree() {
    if !sandbox_ok() {
        eprintln!("skipping: the sandbox cannot run here (see `cowboy doctor`)");
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
    let root = std::fs::canonicalize(proj.path()).unwrap();

    // A live "parent" holds the exclusive lease on the worktree.
    dreq(
        &sock,
        DaemonReq::RegisterWorker {
            info: fake_parent("parent", &root),
        },
    );
    match dreq(
        &sock,
        DaemonReq::AcquireLease {
            key: root.clone(),
            session: "parent".into(),
            mode: LeaseMode::Exclusive,
        },
    ) {
        Some(DaemonResp::LeaseGranted { .. }) => {}
        other => panic!("expected the parent lease to be granted, got {other:?}"),
    }

    // A subagent-style child in the SAME worktree (COWBOY_SUBAGENT_DEPTH set,
    // print-final-only) must run uncoordinated and produce an answer.
    let out = Command::new(env!("CARGO_BIN_EXE_cowboy"))
        .current_dir(proj.path())
        .env("XDG_RUNTIME_DIR", env.runtime.path())
        .env("XDG_STATE_HOME", env.state.path())
        .env("COWBOY_SUBAGENT_DEPTH", "1")
        .env("COWBOY_PRINT_FINAL_ONLY", "1")
        .stdin(std::process::Stdio::null())
        .arg("Reply with exactly the word: ready. Then finish.")
        .output()
        .expect("run subagent-style child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && !stdout.trim().is_empty(),
        "subagent should produce a final answer despite the held lease; status={:?} stdout={:?} stderr={:?}",
        out.status.code(),
        stdout,
        String::from_utf8_lossy(&out.stderr)
    );

    reap_session_residue();
}

/// The flagship end-to-end turn: the daemon starts a session that runs an
/// actual agent turn against the configured model, a client streams it, detach
/// leaves it running, and re-attach replays the journal.
#[test]
#[ignore = "real sandbox + model: needs the kernel prerequisites and ~/.config/cowboy"]
fn e2e_turn_streams_detach_keeps_running_then_reattach_replays() {
    if !sandbox_ok() {
        eprintln!("skipping: the sandbox cannot run here (see `cowboy doctor`)");
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
