//! Sandbox execution tests: real bwrap, real namespaces, real processes.
//!
//! These assert the *confinement* properties the Docker E2E suite used to cover,
//! plus the process-lifecycle guarantees. They need bubblewrap and unprivileged
//! user namespaces, so they self-skip when those are unavailable rather than
//! failing — matching the convention that `--ignored` is safe to run anywhere.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cowboy_cli::sandbox::bwrap::NetMode;
use cowboy_cli::sandbox::exec::{run_streaming, ExecRequest};
use cowboy_core::config::SecurityConfig;
use cowboy_sandbox::plan::{PlanInputs, SandboxPlan};
use cowboy_sandbox::HostProbe;

/// The real host probe, mirroring what the CLI uses.
struct Host;

impl HostProbe for Host {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn git_common_dir(&self, _root: &Path) -> Option<PathBuf> {
        None
    }
    fn expand(&self, raw: &str) -> Option<PathBuf> {
        cowboy_core::config::expand_path(raw).ok()
    }
    fn home(&self) -> Option<PathBuf> {
        cowboy_core::config::expand_path("~").ok()
    }
    fn self_exe(&self) -> Option<PathBuf> {
        // The test binary is not `cowboy`; use the built binary next to it so the
        // shim inside the sandbox is a real cowboy.
        let dir = std::env::current_exe()
            .ok()?
            .parent()?
            .parent()?
            .to_path_buf();
        let exe = dir.join("cowboy");
        exe.exists().then_some(exe)
    }
}

/// Whether the sandbox can run here at all. Returns the reason it cannot, so a
/// skip says why instead of looking like a pass.
fn unsupported() -> Option<String> {
    if cowboy_cli::sandbox::bwrap::resolve_bwrap().is_err() {
        return Some("bubblewrap not available".into());
    }
    if Host.self_exe().is_none() {
        return Some("the cowboy binary is not built alongside the test".into());
    }
    // A trivial sandbox proves unprivileged user namespaces work. The
    // merged-`/usr` symlinks are required, not decoration: without `/lib64` the
    // dynamic linker is missing and even `/usr/bin/true` cannot start, which made
    // this probe report "unsupported" and every test in this file skip while still
    // reporting `ok`.
    let ok = std::process::Command::new("bwrap")
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
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    (!ok).then(|| "unprivileged user namespaces are unavailable".into())
}

/// Skip unless `COWBOY_SANDBOX_TESTS=required`, in which case a skip is a failure.
///
/// Self-skipping keeps `--ignored` safe to run anywhere, but it also means a broken
/// capability probe turns the whole file into a no-op that still reports success.
/// This gives a way to assert the suite really ran.
macro_rules! skip_if_unsupported {
    () => {
        if let Some(why) = unsupported() {
            if std::env::var("COWBOY_SANDBOX_TESTS").as_deref() == Ok("required") {
                panic!("sandbox tests required but unsupported here: {why}");
            }
            eprintln!("skipping: {why}");
            return;
        }
    };
}

/// A project directory with the config a sandbox needs.
struct Project {
    dir: assert_fs::TempDir,
}

impl Project {
    fn new() -> Self {
        let dir = assert_fs::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".cowboy")).unwrap();
        std::fs::write(
            dir.path().join(".cowboy/security.yaml"),
            "version: 1\nSECRET_MARKER_MUST_NOT_BE_READABLE: true\n",
        )
        .unwrap();
        Self { dir }
    }
    fn path(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap()
    }
}

fn mask_file() -> PathBuf {
    let p = std::env::temp_dir().join("cowboy-test-mask");
    std::fs::write(&p, b"").unwrap();
    p
}

fn plan_for(root: &Path) -> SandboxPlan {
    let sec = SecurityConfig::default();
    let mask = mask_file();
    // Real directories: bwrap refuses to bind a source that does not exist.
    let scratch = cowboy_cli::project::ensure_scratch_dir(&cowboy_cli::project::scratch_key(
        "sandbox-exec-test",
    ))
    .unwrap();
    let inputs = PlanInputs {
        root,
        security: &sec,
        grants: &[],
        mask_file: &mask,
        relay_port: 8443,
        scratch: &scratch,
    };
    SandboxPlan::build(&inputs, &Host).unwrap()
}

/// Run a command in a fresh sandbox, returning (exit code, output).
async fn run(root: &Path, command: &str, timeout_secs: u64) -> (i32, String) {
    run_cancellable(
        root,
        command,
        timeout_secs,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
}

async fn run_cancellable(
    root: &Path,
    command: &str,
    timeout_secs: u64,
    cancel: tokio_util::sync::CancellationToken,
) -> (i32, String) {
    let plan = plan_for(root);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let req = ExecRequest {
        plan: &plan,
        command,
        cwd: None,
        timeout_secs,
        net: NetMode::Isolated,
        session: None,
    };
    let (res, out) = run_streaming(req, cancel, tx).await.unwrap();
    (res.exit_code, out)
}

#[tokio::test]
async fn the_project_is_writable_and_is_the_working_directory() {
    skip_if_unsupported!();
    let p = Project::new();
    let (code, out) = run(
        &p.path(),
        "pwd && echo written > proof.txt && cat proof.txt",
        60,
    )
    .await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("/workspace"), "{out}");
    assert!(out.contains("written"), "{out}");
    // The write landed on the host, through the bind.
    assert_eq!(
        std::fs::read_to_string(p.path().join("proof.txt"))
            .unwrap()
            .trim(),
        "written"
    );
}

/// The invariant the Docker suite covered as
/// `security_yaml_is_masked_inside_container`.
#[tokio::test]
async fn host_owned_security_config_is_masked() {
    skip_if_unsupported!();
    let p = Project::new();
    let (code, out) = run(
        &p.path(),
        "cat /workspace/.cowboy/security.yaml; echo \"[end]\"",
        60,
    )
    .await;
    assert_eq!(code, 0, "{out}");
    assert!(
        !out.contains("SECRET_MARKER_MUST_NOT_BE_READABLE"),
        "the agent read host-owned security config: {out}"
    );
    assert!(out.contains("[end]"), "{out}");
}

#[tokio::test]
async fn the_host_toolchain_is_usable() {
    skip_if_unsupported!();
    let p = Project::new();
    let (code, out) = run(
        &p.path(),
        "sh --version >/dev/null 2>&1; command -v git",
        60,
    )
    .await;
    assert_eq!(code, 0, "the host's own binaries should be on PATH: {out}");
    assert!(out.contains("git"), "{out}");
}

/// The host home is exposed only where the plan says so — the user's tool directories,
/// read-only — and is otherwise not reachable.
///
/// Asserted as a property rather than as one error message. It used to check for "No
/// such file", which stopped being the truth once tool directories were bound: bwrap
/// creates `~` as a mount point for them, so the refusal now comes from Landlock
/// instead. Enumeration being refused is the stronger claim anyway, since it means an
/// unexposed path cannot even be discovered.
#[tokio::test]
async fn the_host_home_directory_is_not_browsable() {
    skip_if_unsupported!();
    let p = Project::new();
    let home = cowboy_core::config::expand_path("~").unwrap();

    let (_code, out) = run(
        &p.path(),
        &format!("ls {}/ 2>&1 || true", home.display()),
        60,
    )
    .await;
    assert!(
        out.contains("Permission denied") || out.contains("No such file"),
        "the host home directory must not be enumerable: {out}"
    );

    // Directories that exist on the host and were not exposed stay unreachable, and
    // `~/.local/share` in particular must not be listable — that is where the login
    // keyring lives, and binding `~/.local/share/uv` must not open its parent.
    for sub in [".cache", ".local/share", ".config"] {
        let path = home.join(sub);
        if !path.exists() {
            continue;
        }
        let (_code, out) = run(
            &p.path(),
            &format!("ls {}/ 2>&1 || true", path.display()),
            60,
        )
        .await;
        assert!(
            out.contains("Permission denied") || out.contains("No such file"),
            "~/{sub} was not exposed and must not be reachable: {out}"
        );
    }
}

/// The other half: the tool directories that *are* exposed work, and are read-only.
///
/// Without this the test above could pass by exposing nothing at all, which is exactly
/// the failure mode that made the agent's toolchain differ from its user's.
#[tokio::test]
async fn the_users_own_tools_are_runnable_but_not_writable() {
    skip_if_unsupported!();
    let home = cowboy_core::config::expand_path("~").unwrap();
    let bin = home.join(".local/bin");
    if !bin.exists() {
        eprintln!("skipping: this host has no ~/.local/bin");
        return;
    }
    let p = Project::new();

    let (code, out) = run(&p.path(), &format!("ls {}/ >/dev/null", bin.display()), 60).await;
    assert_eq!(code, 0, "~/.local/bin should be readable: {out}");

    let (_code, out) = run(
        &p.path(),
        &format!("touch {}/EVIL 2>&1 || true", bin.display()),
        60,
    )
    .await;
    assert!(
        out.contains("Read-only file system") || out.contains("Permission denied"),
        "the user's tools must not be writable — that is host code execution on their \
         next shell command: {out}"
    );
    assert!(
        !bin.join("EVIL").exists(),
        "nothing may be created in the user's bin directory"
    );

    // And it is actually on PATH, which is what makes the bind useful.
    let (code, out) = run(&p.path(), "echo $PATH", 60).await;
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains(&bin.display().to_string()),
        "~/.local/bin must be on PATH: {out}"
    );
}

/// A writable root tmpfs would let the agent drop an `/etc/ld.so.preload`.
#[tokio::test]
async fn the_sandbox_root_and_etc_are_read_only() {
    skip_if_unsupported!();
    let p = Project::new();
    let (_code, out) = run(
        &p.path(),
        "for d in / /etc /usr /var; do touch $d/.probe 2>/dev/null && echo \"WRITABLE $d\"; done; echo done",
        60,
    )
    .await;
    assert!(
        !out.contains("WRITABLE"),
        "no part of the system tree may be writable: {out}"
    );
}

#[tokio::test]
async fn exit_codes_propagate() {
    skip_if_unsupported!();
    let p = Project::new();
    let (code, _) = run(&p.path(), "exit 42", 60).await;
    assert_eq!(code, 42);
}

#[tokio::test]
async fn output_is_captured_from_both_streams() {
    skip_if_unsupported!();
    let p = Project::new();
    let (code, out) = run(&p.path(), "echo to-stdout; echo to-stderr 1>&2", 60).await;
    assert_eq!(code, 0);
    assert!(out.contains("to-stdout"), "{out}");
    assert!(
        out.contains("to-stderr"),
        "stderr must be captured too: {out}"
    );
}

#[tokio::test]
async fn a_timeout_stops_the_command() {
    skip_if_unsupported!();
    let p = Project::new();
    let started = Instant::now();
    let (code, out) = run(&p.path(), "sleep 60", 2).await;
    assert_eq!(code, 124, "timeout should report 124: {out}");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the timeout did not actually stop the command"
    );
    assert!(out.contains("timed out"), "{out}");
}

#[tokio::test]
async fn cancellation_stops_the_command() {
    skip_if_unsupported!();
    let p = Project::new();
    let cancel = tokio_util::sync::CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        c.cancel();
    });
    let started = Instant::now();
    let (code, out) = run_cancellable(&p.path(), "sleep 60", 0, cancel).await;
    assert_eq!(code, 130, "cancel should report 130: {out}");
    assert!(started.elapsed() < Duration::from_secs(30));
}

/// The case the Docker path needed a `/proc` env-marker sweep for: a descendant
/// that re-`setsid`s escapes the recorded process group. Here `--unshare-pid` plus
/// `--die-with-parent` means killing bwrap takes down PID 1 of the sandbox
/// namespace and the kernel reaps everything in it.
#[tokio::test]
async fn cancel_reaps_a_descendant_that_escaped_its_process_group() {
    skip_if_unsupported!();
    let p = Project::new();
    // A marker unlikely to collide with anything else on the machine.
    let marker = format!("cowboy-reap-probe-{}", std::process::id());
    let cancel = tokio_util::sync::CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(800)).await;
        c.cancel();
    });
    let command = format!("setsid sh -c 'exec -a {marker} sleep 120' & sleep 120");
    let (code, _out) = run_cancellable(&p.path(), &command, 0, cancel).await;
    assert_eq!(code, 130);

    // Give the kernel a moment to tear the namespace down, then confirm nothing
    // carrying our marker survives anywhere on the host.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        processes_matching(&marker),
        0,
        "a re-setsid'd descendant survived cancellation"
    );
}

/// Count processes whose cmdline contains `needle`, reading `/proc` directly.
///
/// Deliberately not `pgrep -f`, whose pattern also matches the shell that invoked
/// it — a trap that makes such a check silently always non-zero.
fn processes_matching(needle: &str) -> usize {
    let mut n = 0;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(cmd) = std::fs::read(format!("/proc/{name}/cmdline")) {
            if String::from_utf8_lossy(&cmd).contains(needle) {
                n += 1;
            }
        }
    }
    n
}

// ---------------------------------------------------------------------------
// Kernel-level lockdown: Landlock, seccomp, capabilities
// ---------------------------------------------------------------------------

/// Landlock's whole purpose: it is enforced against the *process*, so it still
/// holds when the mount view is wrong.
///
/// This is the only test that can distinguish the two layers. Normally the Landlock
/// rules are derived from the bind list so they agree by construction; here the
/// project is left bound read-write while being removed from the Landlock
/// read-write set, simulating exactly the mistake Landlock exists to contain.
#[tokio::test]
async fn landlock_denies_writes_even_when_the_mount_view_allows_them() {
    skip_if_unsupported!();
    let p = Project::new();
    let root = p.path();

    let mut plan = plan_for(&root);
    // Sanity: the bind really is read-write, so a failure below is Landlock's doing
    // and not the mount view's.
    let bind = plan
        .binds
        .iter()
        .find(|b| b.target == plan.workdir)
        .expect("project bind");
    assert_eq!(bind.mode, cowboy_sandbox::BindMode::ReadWrite);

    // Remove the *sandbox-internal* path: Landlock rules are resolved inside the
    // sandbox, so the workdir is what identifies the project there.
    let workdir = std::path::PathBuf::from(&plan.workdir);
    let before = plan.landlock.read_write.len();
    plan.landlock.read_write.retain(|p| p != &workdir);
    assert_eq!(
        plan.landlock.read_write.len(),
        before - 1,
        "the test must actually remove the project's Landlock rule, or it proves nothing"
    );

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let req = ExecRequest {
        plan: &plan,
        command: "echo nope > /workspace/should-not-exist.txt 2>&1; echo exit=$?",
        cwd: None,
        timeout_secs: 60,
        net: NetMode::Isolated,
        session: None,
    };
    let (_res, out) = run_streaming(req, tokio_util::sync::CancellationToken::new(), tx)
        .await
        .unwrap();
    assert!(
        !root.join("should-not-exist.txt").exists(),
        "Landlock did not prevent the write: {out}"
    );
}

/// The read-only toolchain must still be *executable*, or nothing in `/usr` could
/// run and the sandbox would be useless.
#[tokio::test]
async fn read_only_paths_remain_executable() {
    skip_if_unsupported!();
    let p = Project::new();
    let (code, out) = run(&p.path(), "/usr/bin/env true && echo executed", 60).await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("executed"), "{out}");
}

/// io_uring is the reason the seccomp deny-list exists: operations are submitted as
/// ring entries rather than syscalls, so `IORING_OP_CONNECT` and `IORING_OP_OPENAT`
/// would never pass through a filter on `connect`/`openat`.
///
/// Refusing `io_uring_setup` is what closes that hole, and it closes it completely:
/// with no ring, *no* `IORING_OP_*` is reachable at all. That is why this asserts on
/// ring creation rather than on the individual operations — the operations cannot be
/// attempted without a ring to submit them to.
#[tokio::test]
async fn io_uring_cannot_be_created_so_no_ring_operation_is_reachable() {
    skip_if_unsupported!();
    let p = Project::new();
    // Call the syscall directly; the number is fixed on x86_64 (425).
    let probe = r#"python3 -c '
import ctypes, os
libc = ctypes.CDLL("libc.so.6", use_errno=True)
buf = (ctypes.c_ubyte * 256)()
rc = libc.syscall(425, 8, ctypes.byref(buf))
if rc >= 0:
    print("RING CREATED (BAD)")
else:
    print("io_uring_setup denied:", os.strerror(ctypes.get_errno()))
'"#;
    let (code, out) = run(&p.path(), probe, 60).await;
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("io_uring_setup denied"),
        "io_uring must be unavailable, otherwise the seccomp filter is bypassable: {out}"
    );
    assert!(!out.contains("RING CREATED"), "{out}");
}

#[tokio::test]
async fn raw_sockets_are_denied() {
    skip_if_unsupported!();
    let p = Project::new();
    // SOCK_RAW with flags OR'd in, which an exact-match filter would miss.
    let probe = r#"python3 -c '
import socket
for extra in (0, socket.SOCK_CLOEXEC, socket.SOCK_NONBLOCK):
    try:
        socket.socket(socket.AF_INET, socket.SOCK_RAW | extra, socket.IPPROTO_ICMP)
        print("RAW ALLOWED (BAD) extra=", extra)
    except OSError as e:
        print("raw denied:", e.errno)
'"#;
    let (code, out) = run(&p.path(), probe, 60).await;
    assert_eq!(code, 0, "{out}");
    assert!(!out.contains("RAW ALLOWED"), "{out}");
    assert_eq!(
        out.matches("raw denied").count(),
        3,
        "every SOCK_RAW variant must be denied, including with flags: {out}"
    );
}

/// Ordinary TCP must still work, or the deny-list is too broad to be usable.
#[tokio::test]
async fn ordinary_stream_sockets_still_work() {
    skip_if_unsupported!();
    let p = Project::new();
    let probe = r#"python3 -c '
import socket, threading, time
srv = socket.socket(); srv.bind(("127.0.0.1", 0)); srv.listen(1)
port = srv.getsockname()[1]
threading.Thread(target=lambda: srv.accept(), daemon=True).start()
time.sleep(0.2)
c = socket.socket(); c.settimeout(2); c.connect(("127.0.0.1", port))
print("loopback tcp OK")
'"#;
    let (code, out) = run(&p.path(), probe, 60).await;
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("loopback tcp OK"),
        "the agent must be able to reach services it starts: {out}"
    );
}

/// With an empty capability bounding set and `NO_NEW_PRIVS`, a setuid binary
/// confers nothing. Both matter: an empty effective set alone would still leave a
/// capability regainable.
#[tokio::test]
async fn no_capabilities_and_no_new_privs() {
    skip_if_unsupported!();
    let p = Project::new();
    let (code, out) = run(
        &p.path(),
        "grep -E '^(CapEff|CapBnd|NoNewPrivs):' /proc/self/status",
        60,
    )
    .await;
    assert_eq!(code, 0, "{out}");
    for line in out.lines() {
        if let Some(v) = line.strip_prefix("CapEff:") {
            assert_eq!(
                v.trim().trim_start_matches('0'),
                "",
                "CapEff must be 0: {out}"
            );
        }
        if let Some(v) = line.strip_prefix("CapBnd:") {
            assert_eq!(
                v.trim().trim_start_matches('0'),
                "",
                "CapBnd must be 0 too, or a capability could be regained: {out}"
            );
        }
    }
    assert!(out.contains("NoNewPrivs:\t1"), "{out}");
}

/// The shim refuses to run if capabilities were not dropped, so a future change to
/// the bwrap argv cannot silently hand the agent `CAP_NET_ADMIN` — with which it
/// could rewrite the nftables ruleset that egress policy depends on.
#[tokio::test]
async fn the_shim_refuses_to_run_with_capabilities() {
    skip_if_unsupported!();
    let p = Project::new();
    let _ = &p;
    let shim = Host.self_exe().unwrap();
    // Only `command` is required; everything else defaults.
    let request = serde_json::json!({ "command": "echo should-not-run" });

    // Run the shim directly on the host, where the bounding set is NOT empty.
    let mut child = std::process::Command::new(&shim)
        .arg("x-sandbox-shim")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "the shim should have refused: {stderr}"
    );
    assert!(
        stderr.contains("CapBnd") || stderr.contains("capabilities"),
        "the refusal should name the capability check: {stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("should-not-run"),
        "the command must not have run"
    );
}

/// Upgrading cowboy while a session is live must produce a legible error, not an
/// unrunnable sandbox.
///
/// `cargo install` replaces the binary, after which the running worker's
/// `current_exe()` reads `".../cowboy (deleted)"`. That path is what the plan
/// bind-mounts as the lockdown shim, binds are rendered `--ro-bind-try`, and bwrap
/// *silently skips a missing source* — so nothing landed at the shim path and every
/// command failed with `bwrap: execvp /.cowboy-shim: No such file or directory`. That
/// names the path inside the sandbox and says nothing about the host binary that moved,
/// and it happens for the rest of the session: an agent alive, answering, and unable to
/// run a single command.
///
/// Simulated by pointing the probe at a copy of the binary and then deleting it, so the
/// test never touches the installed one.
#[tokio::test]
async fn a_binary_replaced_mid_session_says_so_instead_of_failing_inside_the_sandbox() {
    skip_if_unsupported!();
    let p = Project::new();
    let real = Host.self_exe().unwrap();
    let tmp = assert_fs::TempDir::new().unwrap();
    let copy = tmp.path().join("cowboy");
    std::fs::copy(&real, &copy).unwrap();

    /// A host whose cowboy binary is the copy above.
    struct MovableHost(PathBuf);
    impl HostProbe for MovableHost {
        fn exists(&self, path: &Path) -> bool {
            path.exists()
        }
        fn git_common_dir(&self, _root: &Path) -> Option<PathBuf> {
            None
        }
        fn expand(&self, raw: &str) -> Option<PathBuf> {
            cowboy_core::config::expand_path(raw).ok()
        }
        fn home(&self) -> Option<PathBuf> {
            cowboy_core::config::expand_path("~").ok()
        }
        fn self_exe(&self) -> Option<PathBuf> {
            Some(self.0.clone())
        }
    }

    let sec = SecurityConfig::default();
    let mask = mask_file();
    let scratch = cowboy_cli::project::ensure_scratch_dir(&cowboy_cli::project::scratch_key(
        "sandbox-exec-replaced",
    ))
    .unwrap();
    let build = |probe: &dyn HostProbe| {
        SandboxPlan::build(
            &PlanInputs {
                root: &p.path(),
                security: &sec,
                grants: &[],
                mask_file: &mask,
                relay_port: 8443,
                scratch: &scratch,
            },
            probe,
        )
        .unwrap()
    };

    let exec = |plan: SandboxPlan| async move {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        run_streaming(
            ExecRequest {
                plan: &plan,
                command: "echo alive",
                cwd: None,
                timeout_secs: 30,
                net: NetMode::Isolated,
                session: None,
            },
            tokio_util::sync::CancellationToken::new(),
            tx,
        )
        .await
    };

    // With the copy in place, the sandbox works.
    let plan = build(&MovableHost(copy.clone()));
    let (res, out) = exec(plan).await.expect("the sandbox should run");
    assert_eq!(res.exit_code, 0, "{out}");
    assert!(out.contains("alive"), "{out}");

    // Now the binary is replaced/removed, exactly as `cargo install` leaves it.
    std::fs::remove_file(&copy).unwrap();
    let plan = build(&MovableHost(copy.clone()));
    let err = exec(plan)
        .await
        .expect_err("a sandbox with no shim must refuse rather than run")
        .to_string();
    assert!(
        err.contains(&copy.display().to_string()),
        "the error must name the host binary that went away: {err}"
    );
    assert!(
        err.contains("upgraded or moved"),
        "and explain how that happens: {err}"
    );
    assert!(
        !err.contains("execvp"),
        "and not surface as bwrap failing to exec a path inside the sandbox: {err}"
    );
}
