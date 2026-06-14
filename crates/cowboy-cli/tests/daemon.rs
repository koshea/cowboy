//! M1: the `cowboyd` daemon binds its socket, answers Ping/ListSessions, and
//! refuses to start a second instance. Isolated via XDG_RUNTIME_DIR/XDG_STATE_HOME.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::time::Duration;

use cowboy_core::daemonproto::{DaemonReq, DaemonRequest, DaemonResp, DaemonResponse};
use cowboy_core::netproto::encode_line;

/// Kill the daemon child on drop so tests never leak processes.
struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_daemon(runtime: &std::path::Path, state: &std::path::Path) -> Daemon {
    let child = Command::new(env!("CARGO_BIN_EXE_cowboyd"))
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", state)
        .spawn()
        .expect("spawn cowboyd");
    Daemon(child)
}

fn request(sock: &std::path::Path, req: DaemonReq) -> Option<DaemonResp> {
    let stream = UnixStream::connect(sock).ok()?;
    let mut w = stream.try_clone().ok()?;
    w.write_all(encode_line(&DaemonRequest { id: 1, req }).as_bytes())
        .ok()?;
    w.flush().ok()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let resp: DaemonResponse = serde_json::from_str(line.trim()).ok()?;
    Some(resp.resp)
}

fn wait_for_pong(sock: &std::path::Path) -> bool {
    for _ in 0..50 {
        if matches!(
            request(sock, DaemonReq::Ping),
            Some(DaemonResp::Pong { .. })
        ) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn daemon_pings_lists_and_is_single_instance() {
    let runtime = assert_fs::TempDir::new().unwrap();
    let state = assert_fs::TempDir::new().unwrap();
    let sock = runtime.path().join("cowboy/cowboyd.sock");

    let _d = spawn_daemon(runtime.path(), state.path());
    assert!(wait_for_pong(&sock), "daemon should answer Ping");

    // Empty registry to start.
    match request(&sock, DaemonReq::ListSessions { root: None }) {
        Some(DaemonResp::Sessions { sessions }) => assert!(sessions.is_empty()),
        other => panic!("expected Sessions, got {other:?}"),
    }

    // A second instance must refuse (lock held) and exit non-zero.
    let second = Command::new(env!("CARGO_BIN_EXE_cowboyd"))
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_STATE_HOME", state.path())
        .output()
        .expect("run second cowboyd");
    assert!(
        !second.status.success(),
        "second cowboyd should refuse to start while the first holds the lock"
    );
}
