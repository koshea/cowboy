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
    ServerMsg, UiEventMsg,
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
        .write_str("version: 1\n")
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
        },
    ) {
        Some(DaemonResp::Started { worker_sock, .. }) => worker_sock,
        other => panic!("expected Started, got {other:?}"),
    }
}

/// A client on a worker's per-session socket.
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
        let mut line = String::new();
        match self.r.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => serde_json::from_str(line.trim()).ok(),
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
    for _ in 0..50 {
        match c.recv() {
            Some(ServerMsg::Event {
                event: UiEventMsg::Notice(m),
                ..
            }) if m.contains(needle) => return true,
            Some(_) => continue,
            None => return false,
        }
    }
    false
}
