//! Worker control-message behavior: interrupting a running turn and switching
//! models. These spawn the real `cowboyd` + worker but need **no Docker and no
//! live model** — the "model" is a TCP blackhole that accepts a connection and
//! never replies, so a turn hangs in the model call until interrupted. The agent
//! only touches Docker when *executing* a tool, which never happens here.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use assert_fs::prelude::*;
use cowboy_core::daemonproto::{
    ClientMsg, DaemonReq, DaemonRequest, DaemonResp, DaemonResponse, InterruptKind, LeaseMode,
    ServerMsg, SessionStatus, UiEventMsg,
};
use cowboy_core::netproto::encode_line;

/// Accept connections and hold them open forever without replying, so an HTTP
/// request to this address blocks. Returns the port; the listener thread (and
/// the accepted streams) stay alive for the process lifetime.
fn blackhole() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming().flatten() {
            held.push(s); // keep open; never write a response
        }
    });
    port
}

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A daemon + project wired to a blackhole "model" endpoint.
struct Fixture {
    _runtime: assert_fs::TempDir,
    _state: assert_fs::TempDir,
    _cfg: assert_fs::TempDir,
    proj: assert_fs::TempDir,
    sock: std::path::PathBuf,
    _daemon: Daemon,
}

fn setup() -> Fixture {
    setup_with_setup_command(false)
}

/// As [`setup`], but with an `agent.setup` command when `eager` — which makes the
/// worker bring the **sandbox** up before the first message. That distinction matters
/// for teardown: a worker that never started a sandbox has no holder process, no
/// namespaces and no broker threads to release, so it cannot exhibit a teardown leak.
fn setup_with_setup_command(eager: bool) -> Fixture {
    let port = blackhole();
    let runtime = assert_fs::TempDir::new().unwrap();
    let state = assert_fs::TempDir::new().unwrap();
    let cfg = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();

    cfg.child("cowboy/providers.yaml")
        .write_str(&format!(
            "version: 1\nproviders:\n  p:\n    base_url: http://127.0.0.1:{port}/v1\n    api_key: k\n"
        ))
        .unwrap();
    cfg.child("cowboy/models.yaml")
        .write_str("version: 1\ndefault: m\nmodels:\n  m:\n    provider: p\n    model: x\n")
        .unwrap();
    proj.child(".cowboy/security.yaml")
        .write_str("version: 1\n")
        .unwrap();
    proj.child(".cowboy/agent.yaml")
        .write_str(if eager {
            "version: 1\nagent:\n  setup:\n    - true\n"
        } else {
            "version: 1\n"
        })
        .unwrap();
    let _ = Command::new("git")
        .arg("-C")
        .arg(proj.path())
        .arg("init")
        .arg("-q")
        .status();

    let sock = runtime.path().join("cowboy/cowboyd.sock");
    let daemon = Command::new(env!("CARGO_BIN_EXE_cowboyd"))
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CONFIG_HOME", cfg.path())
        // A short idle window so the auto-exit tests do not have to wait out the
        // production default.
        .env("COWBOY_DAEMON_LINGER", "1")
        .spawn()
        .expect("spawn cowboyd");
    let fx = Fixture {
        _runtime: runtime,
        _state: state,
        _cfg: cfg,
        proj,
        sock,
        _daemon: Daemon(daemon),
    };
    assert!(wait_pong(&fx.sock), "daemon should answer Ping");
    fx
}

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

fn start(fx: &Fixture, task: Option<&str>) -> std::path::PathBuf {
    match dreq(
        &fx.sock,
        DaemonReq::StartSession {
            root: fx.proj.path().to_path_buf(),
            task: task.map(str::to_string),
            mode: LeaseMode::Exclusive,
            force: false,
            resume: None,
            ranch_id: None,
            workstream_id: None,
        },
    ) {
        Some(DaemonResp::Started { worker_sock, .. }) => worker_sock,
        other => panic!("expected Started, got {other:?}"),
    }
}

/// A client on a worker's per-session socket.
/// One read from a worker socket.
enum Recv {
    // Boxed: this variant is far larger than the others, and the enum is returned
    // from a hot read loop.
    Msg(Box<ServerMsg>),
    /// A line that did not parse as a `ServerMsg`.
    Garbage,
    /// The read timed out; the session is still there.
    Timeout,
    /// EOF or a real error: nothing more is coming.
    Closed,
}

struct Client {
    r: BufReader<UnixStream>,
    w: UnixStream,
}
impl Client {
    fn connect(sock: &Path, read_timeout: Duration) -> Self {
        let s = UnixStream::connect(sock).expect("connect worker socket");
        s.set_read_timeout(Some(read_timeout)).unwrap();
        let w = s.try_clone().unwrap();
        let mut c = Self {
            r: BufReader::new(s),
            w,
        };
        c.send(&ClientMsg::Hello {
            since_seq: None,
            read_only: false,
        });
        assert!(
            matches!(c.recv(), Some(ServerMsg::Snapshot { .. })),
            "first server message should be a Snapshot"
        );
        c
    }
    fn send(&mut self, msg: &ClientMsg) {
        self.w.write_all(encode_line(msg).as_bytes()).unwrap();
        self.w.flush().unwrap();
    }
    fn recv(&mut self) -> Option<ServerMsg> {
        match self.recv_outcome() {
            Recv::Msg(m) => Some(*m),
            Recv::Garbage => None,
            Recv::Timeout | Recv::Closed => None,
        }
    }
    /// As [`Self::recv`], but distinguishing a read timeout from a closed socket.
    ///
    /// The difference matters for anything that waits for a specific message: a
    /// timeout means "not yet", a close means "never", and collapsing them into
    /// `None` turns a slow machine into a test failure.
    fn recv_outcome(&mut self) -> Recv {
        let mut line = String::new();
        match self.r.read_line(&mut line) {
            Ok(0) => Recv::Closed,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Recv::Timeout
            }
            Err(_) => Recv::Closed,
            Ok(_) => match serde_json::from_str(line.trim()) {
                Ok(m) => Recv::Msg(Box::new(m)),
                Err(_) => Recv::Garbage,
            },
        }
    }
}

/// Interrupting a running turn cancels it: the model call hangs on the
/// blackhole, and `Interrupt{Turn}` must unwind it (TurnDone arrives) rather
/// than the turn blocking forever.
#[test]
fn interrupt_cancels_a_running_turn() {
    let fx = setup();
    let ws = start(&fx, Some("do a thing"));
    let mut c = Client::connect(&ws, Duration::from_secs(8));

    // Give the worker a moment to reach the (hanging) model call.
    std::thread::sleep(Duration::from_millis(800));

    let started = Instant::now();
    c.send(&ClientMsg::Interrupt {
        kind: InterruptKind::Turn,
    });

    // The turn must end promptly; without the fix TurnDone never arrives.
    let mut saw_turn_done = false;
    while started.elapsed() < Duration::from_secs(6) {
        match c.recv() {
            Some(ServerMsg::Event {
                event: UiEventMsg::TurnDone,
                ..
            }) => {
                saw_turn_done = true;
                break;
            }
            Some(_) => continue,
            None => break,
        }
    }
    assert!(
        saw_turn_done,
        "interrupt should cancel the hung turn and emit TurnDone"
    );
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "interrupt should be prompt"
    );

    c.send(&ClientMsg::End);
    std::thread::sleep(Duration::from_millis(300));
}

/// `End` must terminate the worker: an idle session that receives `End` finalizes
/// and exits, so the daemon stops reporting it `Running`. (Reproduces the "press
/// e, session stays Running" bug at the protocol level — no TUI involved.)
#[test]
fn end_terminates_the_worker() {
    let fx = setup();
    let ws = start(&fx, None); // idle session (no task)
    let id = ws
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .trim_start_matches("s-")
        .trim_end_matches(".sock")
        .to_string();
    let mut c = Client::connect(&ws, Duration::from_secs(8));
    let worker_pid = match dreq(&fx.sock, DaemonReq::GetSession { id: id.clone() }) {
        Some(DaemonResp::Session { info }) => info.pid,
        other => panic!("expected a session record, got {other:?}"),
    };

    c.send(&ClientMsg::End);

    let started = Instant::now();
    let mut ended = false;
    while started.elapsed() < Duration::from_secs(8) {
        match dreq(&fx.sock, DaemonReq::GetSession { id: id.clone() }) {
            Some(DaemonResp::Session { info }) if info.status != SessionStatus::Running => {
                ended = true;
                break;
            }
            // Reaped from the registry also counts as ended.
            Some(DaemonResp::Err { .. }) => {
                ended = true;
                break;
            }
            _ => std::thread::sleep(Duration::from_millis(150)),
        }
    }
    assert!(
        ended,
        "ClientMsg::End must terminate the worker; still Running after 8s"
    );

    // And the *process* must actually be gone. The daemon learns the session is over
    // BEFORE the worker tears its sandbox down, so a status check alone cannot tell a
    // clean exit from a worker that reported completion and then hung — which is
    // exactly the shape of leak this is guarding against.
    let pid = worker_pid.expect("the daemon should have recorded the worker pid");
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    panic!("the worker process ({pid}) is still alive 10s after End");
}

/// Is a process still *running*?
///
/// Deliberately not `kill(pid, 0)`, which also succeeds for a **zombie** — a process
/// that has exited but whose parent has not reaped it. These tests spawn the daemon
/// and hold its `Child`, so an exited daemon stays a zombie for the rest of the test
/// and `kill(pid, 0)` would report it alive forever. That cost me a false failure
/// here, and the same trap would hide a genuine shutdown bug.
fn pid_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // Field 3 is the state, after the (possibly space-containing) comm in parens.
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .map(|state| state != "Z")
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// End-to-end through the REAL client bridge (not raw protocol): connect the
/// bridge to a live worker, then drop the task sender — exactly what the TUI's
/// "end" does. The worker must terminate. This closes the gap between the
/// protocol-level End test and the full TUI.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_via_bridge_terminates_the_worker() {
    use std::sync::{Arc, Mutex};

    let fx = setup();
    let ws = start(&fx, None); // idle session
    let id = ws
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .trim_start_matches("s-")
        .trim_end_matches(".sock")
        .to_string();

    let (task_tx, task_rx) = std::sync::mpsc::channel::<cowboy_cli::agent::tui::AgentCmd>();
    let (ui_tx, _ui_rx) = std::sync::mpsc::channel::<cowboy_cli::agent::tui::UiEvent>();
    let turn_cancel: cowboy_cli::agent::tui::TurnCancel =
        Arc::new(Mutex::new(Some(tokio_util::sync::CancellationToken::new())));
    let stream = tokio::net::UnixStream::connect(&ws).await.unwrap();
    let bridge_h = tokio::spawn(cowboy_cli::cmd::attach::bridge(
        stream,
        ui_tx,
        task_rx,
        turn_cancel,
        false,
    ));

    // Let the bridge connect + send Hello, then simulate pressing "e" (end).
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(task_tx);

    let started = Instant::now();
    let mut ended = false;
    while started.elapsed() < Duration::from_secs(8) {
        match dreq(&fx.sock, DaemonReq::GetSession { id: id.clone() }) {
            Some(DaemonResp::Session { info }) if info.status != SessionStatus::Running => {
                ended = true;
                break;
            }
            Some(DaemonResp::Err { .. }) => {
                ended = true;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(150)).await,
        }
    }
    bridge_h.abort();
    assert!(
        ended,
        "dropping the bridge's task sender (TUI 'end') must terminate the worker"
    );
}

/// `Detach` closes *that* client's connection promptly but leaves the session
/// running: the worker breaks its per-client serve loop (so the client's read
/// EOFs without waiting on a turn or an `Ended` that never comes), and a fresh
/// client can still attach. This is the worker half of the pause-menu "detach".
#[test]
fn detach_closes_client_but_keeps_session_alive() {
    let fx = setup();
    let ws = start(&fx, None);
    let mut a = Client::connect(&ws, Duration::from_secs(8));

    // Detach: the worker should close this connection (EOF) well before the
    // 8s read timeout — i.e. a real close, not a timeout.
    let started = Instant::now();
    a.send(&ClientMsg::Detach);
    assert!(
        a.recv().is_none(),
        "detach should close the client connection (EOF)"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "detach close should be prompt, not the read timeout"
    );

    // The session is still alive: a new client attaches and gets a Snapshot.
    let mut b = Client::connect(&ws, Duration::from_secs(8));
    b.send(&ClientMsg::End);
    std::thread::sleep(Duration::from_millis(300));
}

/// `SwitchModel` swaps the model when the name resolves and reports a failure
/// (without crashing the session) when it doesn't. Exercised while idle (no
/// turn), so no model call happens.
#[test]
fn switch_model_reports_success_and_failure() {
    let fx = setup();
    let ws = start(&fx, None);
    let mut c = Client::connect(&ws, Duration::from_secs(8));

    // Unknown model -> a failure notice, session stays alive.
    c.send(&ClientMsg::SwitchModel("does-not-exist".into()));
    assert!(
        wait_for_notice(&mut c, "switch failed"),
        "unknown model should report a switch failure"
    );

    // Known model -> a success notice.
    c.send(&ClientMsg::SwitchModel("m".into()));
    assert!(
        wait_for_notice(&mut c, "switched to model m"),
        "known model should switch"
    );

    c.send(&ClientMsg::End);
    std::thread::sleep(Duration::from_millis(300));
}

/// Read events until a `Notice` containing `needle` (or we run out / time out).
fn wait_for_notice(c: &mut Client, needle: &str) -> bool {
    // Bounded by time, not by a message count: a busy machine can interleave far more
    // than fifty events before the one being waited for, and a read timeout is not the
    // same as the session hanging up.
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match c.recv_outcome() {
            Recv::Msg(m)
                if matches!(
                    &*m,
                    ServerMsg::Event { event: UiEventMsg::Notice(n), .. } if n.contains(needle)
                ) =>
            {
                return true
            }
            Recv::Msg(_) | Recv::Garbage | Recv::Timeout => continue,
            Recv::Closed => return false,
        }
    }
    false
}

/// A client that **vanishes** — no `End`, no `Detach`, just a closed socket — must not
/// strand the session. This is the failure that kept coming back: `End` travels over
/// the socket, so a client killed outright, a closed terminal, or a client that lost
/// the race in its own shutdown all left the worker parked in its idle loop forever,
/// holding a sandbox holder and keeping the daemon alive with it.
///
/// The worker cannot see *why* the socket closed, so the distinction is drawn at the
/// protocol: leaving on purpose says `Detach` (asserted by the test below), and
/// anything else is a client that is not coming back.
#[test]
fn a_client_that_vanishes_without_detaching_ends_the_session() {
    let fx = setup();
    let ws = start(&fx, None);
    let id = ws
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .trim_start_matches("s-")
        .trim_end_matches(".sock")
        .to_string();
    let worker_pid = match dreq(&fx.sock, DaemonReq::GetSession { id: id.clone() }) {
        Some(DaemonResp::Session { info }) => info.pid.expect("a worker pid"),
        other => panic!("expected a session record, got {other:?}"),
    };

    // Attach, then drop the connection on the floor — the moral equivalent of the
    // client being SIGKILLed.
    let c = Client::connect(&ws, Duration::from_secs(8));
    drop(c);

    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(40) {
        if !pid_alive(worker_pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("the worker ({worker_pid}) outlived a client that vanished without detaching");
}

/// …and the converse, which is what makes the rule above safe: a client that
/// *detaches* is saying "keep going", so the session must stay up and reattachable.
/// Without this the fix above would quietly delete the detach feature.
#[test]
fn a_client_that_detaches_leaves_the_session_running() {
    let fx = setup();
    let ws = start(&fx, None);
    let id = ws
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .trim_start_matches("s-")
        .trim_end_matches(".sock")
        .to_string();
    let worker_pid = match dreq(&fx.sock, DaemonReq::GetSession { id: id.clone() }) {
        Some(DaemonResp::Session { info }) => info.pid.expect("a worker pid"),
        other => panic!("expected a session record, got {other:?}"),
    };

    let mut c = Client::connect(&ws, Duration::from_secs(8));
    c.send(&ClientMsg::Detach);
    drop(c);

    // Well past the abandonment grace period.
    std::thread::sleep(Duration::from_secs(12));
    assert!(
        pid_alive(worker_pid),
        "a detached session must stay running and reattachable"
    );
    assert!(ws.exists(), "its socket must stay connectable");

    // And it really is reattachable, not merely alive.
    let mut c2 = Client::connect(&ws, Duration::from_secs(8));
    c2.send(&ClientMsg::End);
}

/// The leak this is really about: a worker that **started a sandbox** must still exit
/// on `End`, and must take its holder process with it.
///
/// The holder owns the session's namespaces, its interception ruleset and its cgroup,
/// so a surviving holder is not merely an untidy process — it is a live sandbox with
/// nobody driving it. `agent.setup` forces the eager bring-up that creates one.
#[test]
fn end_terminates_a_worker_that_started_a_sandbox_and_its_holder() {
    if !sandbox_available() {
        eprintln!("skipping: the sandbox cannot run here (see `cowboy doctor`)");
        return;
    }
    let fx = setup_with_setup_command(true);
    let ws = start(&fx, None);
    let id = ws
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .trim_start_matches("s-")
        .trim_end_matches(".sock")
        .to_string();
    let mut c = Client::connect(&ws, Duration::from_secs(8));
    let worker_pid = match dreq(&fx.sock, DaemonReq::GetSession { id: id.clone() }) {
        Some(DaemonResp::Session { info }) => info.pid.expect("a worker pid"),
        other => panic!("expected a session record, got {other:?}"),
    };

    // Wait for a holder to exist, so this cannot pass by the sandbox never coming up.
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(20) && holders_of(worker_pid).is_empty() {
        std::thread::sleep(Duration::from_millis(200));
    }
    let holders = holders_of(worker_pid);
    assert!(
        !holders.is_empty(),
        "the sandbox never came up, so this test would prove nothing"
    );

    c.send(&ClientMsg::End);

    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(20) {
        let worker_gone = !pid_alive(worker_pid);
        let holders_gone = holders.iter().all(|h| !pid_alive(*h));
        if worker_gone && holders_gone {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!(
        "after End: worker {worker_pid} alive={}, holders {holders:?} alive={:?}",
        pid_alive(worker_pid),
        holders.iter().map(|h| pid_alive(*h)).collect::<Vec<_>>()
    );
}

/// Whether a sandbox can run here at all.
fn sandbox_available() -> bool {
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

/// PIDs of `x-sandbox-holder` processes descended from `worker`.
///
/// Read from `/proc` rather than with `pgrep -f`, whose pattern would also match the
/// process running the search.
fn holders_of(worker: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(cmdline) = std::fs::read_to_string(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if !cmdline.contains("x-sandbox-holder") {
            continue;
        }
        // `unshare` sits between the worker and the holder, so match the whole
        // ancestor chain rather than only the direct parent.
        let mut cur = pid;
        for _ in 0..6 {
            let Some(parent) = ppid_of(cur) else { break };
            if parent == worker {
                out.push(pid);
                break;
            }
            if parent <= 1 {
                break;
            }
            cur = parent;
        }
    }
    out
}

fn ppid_of(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("PPid:"))
        .and_then(|v| v.trim().parse().ok())
}

/// With no sessions left, `cowboyd` exits on its own — so quitting the last TUI
/// leaves nothing behind. It restarts on the next `cowboy` command, so lingering
/// bought nothing but a surprising process in `ps`.
///
/// `COWBOY_DAEMON_LINGER` shortens the grace period; the point of the grace period
/// is that a just-started daemon sees zero sessions for the moment before its client
/// registers one, so exiting instantly would race.
#[test]
fn the_daemon_exits_once_no_sessions_remain() {
    let fx = setup();
    let pid = daemon_pid(&fx.sock).expect("the daemon should be reachable");

    // Run a session to completion, so this covers "the last TUI quit" rather than
    // only "a daemon nobody ever used".
    let ws = start(&fx, None);
    let mut c = Client::connect(&ws, Duration::from_secs(8));
    c.send(&ClientMsg::End);

    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(60) {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("cowboyd ({pid}) is still alive 60s after the last session ended");
}

/// A **detached** session must keep the daemon alive: the worker is still serving and
/// the user can reattach, so exiting would orphan it from its control plane.
#[test]
fn the_daemon_stays_up_while_a_session_is_detached() {
    let fx = setup();
    let pid = daemon_pid(&fx.sock).expect("the daemon should be reachable");

    let ws = start(&fx, None);
    let mut c = Client::connect(&ws, Duration::from_secs(8));
    c.send(&ClientMsg::Detach);
    drop(c);

    // Well past the linger window this fixture sets.
    std::thread::sleep(Duration::from_secs(12));
    assert!(
        pid_alive(pid),
        "cowboyd must not exit while a detached session is live"
    );
}

/// The daemon's pid, via its own `Ping`.
fn daemon_pid(sock: &Path) -> Option<u32> {
    match dreq(sock, DaemonReq::Ping) {
        Some(DaemonResp::Pong { pid, .. }) => Some(pid),
        _ => None,
    }
}

/// An ended session must not be attachable — the symptom that kept coming back.
///
/// The old failure was subtle: `End` told *currently attached* clients the session was
/// over, but left the accept loop running and the socket file on disk. A client that
/// connected afterwards was accepted by a worker already tearing down, so it looked
/// like it had joined a live session and then hung. The daemon's registry said
/// `Completed` the whole time, which is why this was easy to blame on something else.
///
/// Asserted at the socket, not through the registry, because the socket is what a
/// stale client actually connects to.
#[test]
fn an_ended_session_is_not_attachable() {
    let fx = setup();
    let ws = start(&fx, None);
    let mut c = Client::connect(&ws, Duration::from_secs(8));

    c.send(&ClientMsg::End);
    drop(c);

    // PROMPTLY is the assertion. The daemon's periodic vacuum also prunes dead
    // sockets, so a generous deadline here would pass on the old behaviour too — it
    // just took ten seconds, and those ten seconds were the bug: long enough to
    // reattach to a session you had just ended. This deadline is far below the
    // vacuum's interval and far above what the worker needs.
    let deadline = Duration::from_secs(3);
    let started = Instant::now();
    while started.elapsed() < deadline && ws.exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !ws.exists(),
        "the worker socket still existed {:?} after End — long enough for a client to \
         attach to an ended session",
        started.elapsed()
    );

    // And connecting is refused rather than accepted-then-hung.
    match UnixStream::connect(&ws) {
        Err(_) => {}
        Ok(_) => panic!("connecting to an ended session succeeded; it must be refused"),
    }
}
