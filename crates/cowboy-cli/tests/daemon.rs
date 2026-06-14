//! M1: the `cowboyd` daemon binds its socket, answers Ping/ListSessions, and
//! refuses to start a second instance. Isolated via XDG_RUNTIME_DIR/XDG_STATE_HOME.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use assert_fs::prelude::*;

use cowboy_core::daemonproto::{
    AttachTarget, DaemonReq, DaemonRequest, DaemonResp, DaemonResponse, LeaseMode, SessionInfo,
    SessionStatus,
};
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

fn sample_info(id: &str) -> SessionInfo {
    SessionInfo {
        id: id.into(),
        root: "/home/me/app".into(),
        task: Some("fix tests".into()),
        status: SessionStatus::Running,
        pid: Some(1234),
        branch: Some("main".into()),
        container_name: Some("cowboy-agent-app-deadbeef".into()),
        worker_sock: Some("/tmp/cowboy-x/s-abc.sock".into()),
        journal_path: Some("/home/me/app/.cowboy/sessions/abc/events.jsonl".into()),
        lease_mode: Some(LeaseMode::Exclusive),
        started_ms: 1,
        last_heartbeat_ms: 1,
        turn: 0,
        tokens: (0, 0),
        attached_clients: 0,
        diffstat: String::new(),
        running_command: None,
    }
}

#[test]
fn registry_register_attach_complete() {
    let runtime = assert_fs::TempDir::new().unwrap();
    let state = assert_fs::TempDir::new().unwrap();
    let sock = runtime.path().join("cowboy/cowboyd.sock");
    let _d = spawn_daemon(runtime.path(), state.path());
    assert!(wait_for_pong(&sock));

    // Register a worker; it should appear in the listing + GetSession.
    let info = sample_info("sess-1");
    assert!(matches!(
        request(&sock, DaemonReq::RegisterWorker { info: info.clone() }),
        Some(DaemonResp::Registered)
    ));
    match request(&sock, DaemonReq::ListSessions { root: None }) {
        Some(DaemonResp::Sessions { sessions }) => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].id, "sess-1");
        }
        other => panic!("expected Sessions, got {other:?}"),
    }

    // A running session attaches Live to its worker socket.
    match request(
        &sock,
        DaemonReq::AttachSession {
            id: "sess-1".into(),
        },
    ) {
        Some(DaemonResp::Attach {
            target: AttachTarget::Live { worker_sock },
        }) => assert_eq!(worker_sock, info.worker_sock.unwrap()),
        other => panic!("expected Live attach, got {other:?}"),
    }

    // After completion it becomes terminal and attaches as a journal Replay.
    assert!(matches!(
        request(
            &sock,
            DaemonReq::CompleteSession {
                id: "sess-1".into()
            }
        ),
        Some(DaemonResp::Completed)
    ));
    match request(
        &sock,
        DaemonReq::AttachSession {
            id: "sess-1".into(),
        },
    ) {
        Some(DaemonResp::Attach {
            target: AttachTarget::Replay { status, .. },
        }) => assert_eq!(status, SessionStatus::Completed),
        other => panic!("expected Replay attach, got {other:?}"),
    }

    // State persisted to disk.
    let state_file = state.path().join("cowboy/daemon/state.json");
    assert!(state_file.exists(), "state.json should be written");
}

fn git(repo: &std::path::Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("run git")
        .status
        .success();
    assert!(ok, "git {args:?} failed");
}

/// CreateWorktree/ListWorktrees over the wire: the daemon shells out to git and
/// the new `cowboy/<slug>` branch shows up in the listing.
#[test]
fn create_and_list_worktrees_over_the_wire() {
    let repo = assert_fs::TempDir::new().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@t"]);
    git(repo.path(), &["config", "user.name", "t"]);
    std::fs::write(repo.path().join("README"), "hi").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "init"]);

    let runtime = assert_fs::TempDir::new().unwrap();
    let state = assert_fs::TempDir::new().unwrap();
    let sock = runtime.path().join("cowboy/cowboyd.sock");
    let _d = spawn_daemon(runtime.path(), state.path());
    assert!(wait_for_pong(&sock));

    let path = match request(
        &sock,
        DaemonReq::CreateWorktree {
            repo: repo.path().to_path_buf(),
            branch: "cowboy/feature".into(),
            path: None,
        },
    ) {
        Some(DaemonResp::WorktreeCreated { path, branch }) => {
            assert_eq!(branch, "cowboy/feature");
            path
        }
        other => panic!("expected WorktreeCreated, got {other:?}"),
    };
    assert!(path.exists(), "worktree dir should exist");

    match request(
        &sock,
        DaemonReq::ListWorktrees {
            repo: repo.path().to_path_buf(),
        },
    ) {
        Some(DaemonResp::Worktrees { list }) => {
            assert!(
                list.iter().any(|w| w.branch == "cowboy/feature"),
                "listing should include the new worktree: {list:?}"
            );
        }
        other => panic!("expected Worktrees, got {other:?}"),
    }
}

/// On restart the daemon reconciles `state.json`: a session whose worker is
/// gone (here, its worktree root no longer exists) is marked `Stale`.
#[test]
fn daemon_reconciles_dead_sessions_on_restart() {
    let runtime = assert_fs::TempDir::new().unwrap();
    let state = assert_fs::TempDir::new().unwrap();
    let sock = runtime.path().join("cowboy/cowboyd.sock");

    {
        let _d = spawn_daemon(runtime.path(), state.path());
        assert!(wait_for_pong(&sock));
        // sample_info's root (/home/me/app) does not exist, so on the next
        // daemon load this session reconciles to Stale.
        assert!(matches!(
            request(
                &sock,
                DaemonReq::RegisterWorker {
                    info: sample_info("zombie")
                }
            ),
            Some(DaemonResp::Registered)
        ));
        // It starts out Running.
        match request(
            &sock,
            DaemonReq::GetSession {
                id: "zombie".into(),
            },
        ) {
            Some(DaemonResp::Session { info }) => assert_eq!(info.status, SessionStatus::Running),
            other => panic!("expected Session, got {other:?}"),
        }
        // _d dropped here -> daemon killed.
    }

    // Restart against the same state dir.
    let _d2 = spawn_daemon(runtime.path(), state.path());
    assert!(wait_for_pong(&sock));
    match request(
        &sock,
        DaemonReq::GetSession {
            id: "zombie".into(),
        },
    ) {
        Some(DaemonResp::Session { info }) => assert_eq!(info.status, SessionStatus::Stale),
        other => panic!("expected Stale session after restart, got {other:?}"),
    }
}

/// `CleanupStale` over the wire reaps a stale session and frees its lease.
#[test]
fn cleanup_stale_reaps_over_the_wire() {
    let runtime = assert_fs::TempDir::new().unwrap();
    let state = assert_fs::TempDir::new().unwrap();
    let sock = runtime.path().join("cowboy/cowboyd.sock");
    let _d = spawn_daemon(runtime.path(), state.path());
    assert!(wait_for_pong(&sock));

    // A session whose root is gone, holding a lease.
    request(
        &sock,
        DaemonReq::RegisterWorker {
            info: sample_info("z"),
        },
    );
    let key = std::path::PathBuf::from("/home/me/app");
    assert!(matches!(
        request(
            &sock,
            DaemonReq::AcquireLease {
                key: key.clone(),
                session: "z".into(),
                mode: LeaseMode::Exclusive,
            },
        ),
        Some(DaemonResp::LeaseGranted { .. })
    ));

    // Cleanup sweeps it stale (root gone), reaps the record, frees the lease.
    match request(&sock, DaemonReq::CleanupStale { dry_run: false }) {
        Some(DaemonResp::CleanedUp {
            reclaimed,
            leases_released,
        }) => {
            assert_eq!(reclaimed, vec!["z".to_string()]);
            assert_eq!(leases_released, vec![key]);
        }
        other => panic!("expected CleanedUp, got {other:?}"),
    }
    // The record is gone.
    assert!(matches!(
        request(&sock, DaemonReq::GetSession { id: "z".into() }),
        Some(DaemonResp::Err { .. })
    ));
}

/// AcquireLease/ReleaseLease over the wire: a live holder blocks a second
/// acquirer (LeaseDenied), and a release frees the worktree again.
#[test]
fn lease_acquire_denies_live_holder_then_release_frees_it() {
    let runtime = assert_fs::TempDir::new().unwrap();
    let state = assert_fs::TempDir::new().unwrap();
    let sock = runtime.path().join("cowboy/cowboyd.sock");
    let _d = spawn_daemon(runtime.path(), state.path());
    assert!(wait_for_pong(&sock));

    // A live session s1 holds the lease on /home/me/app.
    let info = sample_info("s1");
    assert!(matches!(
        request(&sock, DaemonReq::RegisterWorker { info: info.clone() }),
        Some(DaemonResp::Registered)
    ));
    let key = std::path::PathBuf::from("/home/me/app");
    assert!(matches!(
        request(
            &sock,
            DaemonReq::AcquireLease {
                key: key.clone(),
                session: "s1".into(),
                mode: LeaseMode::Exclusive,
            },
        ),
        Some(DaemonResp::LeaseGranted { .. })
    ));

    // s2 is refused while s1 is live, and learns who holds it.
    match request(
        &sock,
        DaemonReq::AcquireLease {
            key: key.clone(),
            session: "s2".into(),
            mode: LeaseMode::Exclusive,
        },
    ) {
        Some(DaemonResp::LeaseDenied { held_by, .. }) => assert_eq!(held_by.id, "s1"),
        other => panic!("expected LeaseDenied, got {other:?}"),
    }

    // After s1 releases, s2 can take the worktree.
    assert!(matches!(
        request(
            &sock,
            DaemonReq::ReleaseLease {
                key: key.clone(),
                session: "s1".into(),
            },
        ),
        Some(DaemonResp::Updated)
    ));
    assert!(matches!(
        request(
            &sock,
            DaemonReq::AcquireLease {
                key,
                session: "s2".into(),
                mode: LeaseMode::Exclusive,
            },
        ),
        Some(DaemonResp::LeaseGranted { .. })
    ));
}

/// A piped, non-interactive `cowboy "task"` refuses to run in a worktree that a
/// live session already holds (it fails before any model call). No Docker/model
/// needed — coordination bails at lease acquisition.
#[test]
fn piped_run_is_denied_in_a_busy_worktree() {
    let runtime = assert_fs::TempDir::new().unwrap();
    let state = assert_fs::TempDir::new().unwrap();
    let cfg = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();

    // A model provider so the piped run gets as far as lease acquisition.
    cfg.child("cowboy/providers.yaml")
        .write_str("version: 1\nproviders:\n  p:\n    base_url: https://x/v1\n    api_key: k\n")
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

    let sock = runtime.path().join("cowboy/cowboyd.sock");
    let daemon = Command::new(env!("CARGO_BIN_EXE_cowboyd"))
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CONFIG_HOME", cfg.path())
        .spawn()
        .expect("spawn cowboyd");
    let _d = Daemon(daemon);
    assert!(wait_for_pong(&sock));

    // A live session (pid = this test process) holds the worktree lease.
    let mut holder = sample_info("holder");
    holder.root = proj.path().to_path_buf();
    holder.pid = Some(std::process::id());
    assert!(matches!(
        request(&sock, DaemonReq::RegisterWorker { info: holder }),
        Some(DaemonResp::Registered)
    ));
    assert!(matches!(
        request(
            &sock,
            DaemonReq::AcquireLease {
                key: proj.path().to_path_buf(),
                session: "holder".into(),
                mode: LeaseMode::Exclusive,
            },
        ),
        Some(DaemonResp::LeaseGranted { .. })
    ));

    // A piped `cowboy "task"` in the same worktree must fail safe (non-zero exit,
    // explanatory message) without running a turn.
    let out = Command::new(env!("CARGO_BIN_EXE_cowboy"))
        .current_dir(proj.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CONFIG_HOME", cfg.path())
        .arg("do something")
        .stdin(Stdio::null())
        .output()
        .expect("run piped cowboy");
    assert!(
        !out.status.success(),
        "piped run should fail in a busy worktree"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("active session") || stderr.contains("share a worktree"),
        "expected a busy-worktree message, got: {stderr}"
    );
}

#[test]
fn start_session_spawns_and_registers_a_worker() {
    let runtime = assert_fs::TempDir::new().unwrap();
    let state = assert_fs::TempDir::new().unwrap();
    let cfg = assert_fs::TempDir::new().unwrap(); // XDG_CONFIG_HOME (providers/models)
    let proj = assert_fs::TempDir::new().unwrap();

    // Host-owned provider + a model.
    cfg.child("cowboy/providers.yaml")
        .write_str("version: 1\nproviders:\n  p:\n    base_url: https://x/v1\n    api_key: k\n")
        .unwrap();
    cfg.child("cowboy/models.yaml")
        .write_str("version: 1\ndefault: m\nmodels:\n  m:\n    provider: p\n    model: x\n")
        .unwrap();
    // Minimal project config.
    proj.child(".cowboy/security.yaml")
        .write_str("version: 1\n")
        .unwrap();
    proj.child(".cowboy/agent.yaml")
        .write_str("version: 1\n")
        .unwrap();

    let sock = runtime.path().join("cowboy/cowboyd.sock");
    let daemon = Command::new(env!("CARGO_BIN_EXE_cowboyd"))
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CONFIG_HOME", cfg.path())
        .spawn()
        .expect("spawn cowboyd");
    let _d = Daemon(daemon);
    assert!(wait_for_pong(&sock));

    // Start a session with no task (so the worker registers but runs no turn —
    // no Docker/model needed).
    let (id, worker_sock) = match request(
        &sock,
        DaemonReq::StartSession {
            root: proj.path().to_path_buf(),
            task: None,
            mode: LeaseMode::Exclusive,
            force: false,
            resume: None,
        },
    ) {
        Some(DaemonResp::Started { id, worker_sock }) => (id, worker_sock),
        other => panic!("expected Started, got {other:?}"),
    };
    assert!(worker_sock.exists(), "worker should bind its socket");

    // The worker registered itself; it shows up in the listing.
    match request(&sock, DaemonReq::ListSessions { root: None }) {
        Some(DaemonResp::Sessions { sessions }) => {
            assert!(sessions.iter().any(|s| s.id == id), "session {id} missing");
        }
        other => panic!("expected Sessions, got {other:?}"),
    }

    // Tell the worker to end so it doesn't linger.
    if let Ok(mut w) = UnixStream::connect(&worker_sock) {
        let _ = w.write_all(
            encode_line(&cowboy_core::daemonproto::ClientMsg::Hello {
                since_seq: None,
                read_only: false,
            })
            .as_bytes(),
        );
        let _ = w.write_all(encode_line(&cowboy_core::daemonproto::ClientMsg::End).as_bytes());
        let _ = w.flush();
    }
    std::thread::sleep(Duration::from_millis(500));
}
