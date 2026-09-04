//! Session sandbox tests: real namespaces held across multiple commands.
//!
//! The properties here are the ones that make a session worth having — a service
//! started by one command is reachable from the next — and the ones that keep
//! commands isolated from each other despite that sharing.
//!
//! Self-skips when bubblewrap or unprivileged user namespaces are unavailable.
//! `COWBOY_SANDBOX_TESTS=required` turns a skip into a failure, which is the guard
//! against the whole file silently passing while doing nothing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use cowboy_cli::sandbox::native::NativeSandbox;
use cowboy_cli::sandbox::Sandbox;
use cowboy_core::config::{ProcessDef, SecurityConfig};
use cowboy_sandbox::HostProbe;

/// The real host, optionally with a faked home directory.
///
/// Faking the home is what lets a test put a credential store on disk and assert the
/// denylist refuses it, without writing anything into the developer's actual `~`.
struct Host {
    home: Option<PathBuf>,
}

impl Host {
    fn real() -> Self {
        Self { home: None }
    }
    fn with_home(home: PathBuf) -> Self {
        Self { home: Some(home) }
    }
}

impl HostProbe for Host {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn git_common_dir(&self, _root: &Path) -> Option<PathBuf> {
        None
    }
    /// Must agree with [`Self::home`]: the denylist resolves `~/.aws` and friends
    /// through here, so a faked home that this ignored would silently shrink the
    /// denylist — which is exactly the bug the first version of this had.
    fn expand(&self, raw: &str) -> Option<PathBuf> {
        match (&self.home, raw.strip_prefix("~/")) {
            (Some(h), Some(rest)) => Some(h.join(rest)),
            (Some(h), None) if raw == "~" => Some(h.clone()),
            _ => cowboy_core::config::expand_path(raw).ok(),
        }
    }
    fn home(&self) -> Option<PathBuf> {
        match &self.home {
            Some(h) => Some(h.clone()),
            None => cowboy_core::config::expand_path("~").ok(),
        }
    }
    fn self_exe(&self) -> Option<PathBuf> {
        let dir = std::env::current_exe()
            .ok()?
            .parent()?
            .parent()?
            .to_path_buf();
        let exe = dir.join("cowboy");
        exe.exists().then_some(exe)
    }
}

fn unsupported() -> Option<String> {
    if cowboy_cli::sandbox::bwrap::resolve_bwrap().is_err() {
        return Some("bubblewrap not available".into());
    }
    if Host::real().self_exe().is_none() {
        return Some("the cowboy binary is not built alongside the test".into());
    }
    if which("unshare").is_none() {
        return Some("util-linux `unshare` not available".into());
    }
    if which("ip").is_none() {
        return Some("iproute2 `ip` not available".into());
    }
    // The merged-/usr symlinks are required: without /lib64 the dynamic linker is
    // missing and even /usr/bin/true cannot start.
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

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

/// Whether to skip a test needing a real cgroup.
///
/// Not gated on `COWBOY_SANDBOX_TESTS=required`: that switch means the security
/// boundary must work here, and resource limits are explicitly not part of it. A CI
/// runner has no delegated subtree and should still be able to demand a boundary.
/// `COWBOY_CGROUP_TESTS=required` is the narrower switch, for a host that has
/// delegation and wants to notice if it silently loses it.
fn skip_no_cgroups() -> bool {
    if cowboy_cli::sandbox::cgroup::available() {
        return false;
    }
    assert!(
        std::env::var("COWBOY_CGROUP_TESTS").as_deref() != Ok("required"),
        "COWBOY_CGROUP_TESTS=required but no usable cgroup v2 subtree on this host"
    );
    eprintln!("skipping: no usable cgroup v2 subtree on this host");
    true
}

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

struct Project {
    dir: assert_fs::TempDir,
}

impl Project {
    fn new() -> Self {
        let dir = assert_fs::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".cowboy")).unwrap();
        std::fs::write(dir.path().join(".cowboy/security.yaml"), "version: 1\n").unwrap();
        Self { dir }
    }
    fn path(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap()
    }
}

fn sandbox(root: &Path) -> NativeSandbox {
    sandbox_with_store(root).0
}

/// A sandbox whose persisted-grant store is a temp directory.
///
/// The returned `TempDir` must be kept alive for the test: it is what stops the test
/// reading the developer's real global grants (which would change what the sandbox
/// can see from machine to machine) or writing into their config dir.
fn sandbox_with_store(root: &Path) -> (NativeSandbox, assert_fs::TempDir) {
    sandbox_with_probe(root, Host::real())
}

fn sandbox_with_probe(root: &Path, probe: Host) -> (NativeSandbox, assert_fs::TempDir) {
    sandbox_with(root, probe, SecurityConfig::default())
}

fn sandbox_with(
    root: &Path,
    probe: Host,
    security: SecurityConfig,
) -> (NativeSandbox, assert_fs::TempDir) {
    let store = assert_fs::TempDir::new().unwrap();
    // DenyAll: these tests exercise the sandbox lifecycle, not policy, and an
    // explicit fail-closed approver keeps them from depending on a UI.
    let s = NativeSandbox::new(
        root.to_path_buf(),
        security,
        Box::new(probe),
        std::sync::Arc::new(cowboy_gateway::DenyAll),
    )
    .unwrap()
    .with_grants_dir(store.path().to_path_buf());
    (s, store)
}

async fn run(s: &NativeSandbox, command: &str) -> (i32, String) {
    let (res, out) = s.run_capture(command, None, 120).await.unwrap();
    (res.exit_code, out)
}

/// The reason a session exists: a service started by one command must be reachable
/// from the next, which needs a shared network namespace.
#[tokio::test]
async fn a_background_process_is_reachable_from_a_later_command() {
    skip_if_unsupported!();
    let p = Project::new();
    let s = sandbox(&p.path());

    let def = ProcessDef {
        // A trivial listener; `nc`-free so it depends only on python3.
        command: "python3 -c 'import socket,time
srv=socket.socket(); srv.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
srv.bind((\"127.0.0.1\",18080)); srv.listen(8)
while True:
    c,_=srv.accept(); c.sendall(b\"hello-from-process\"); c.close()'"
            .to_string(),
        cwd: "/workspace".to_string(),
        auto_start: false,
    };
    s.start_process("web", &def).await.unwrap();
    assert!(
        s.process_is_running("web"),
        "the process should have been started"
    );

    // Poll rather than sleep-then-connect. A fixed wait was flaky: on a loaded
    // machine python3's startup inside a fresh sandbox can outlast any constant
    // small enough to keep the suite quick.
    let mut out = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        let (code, text) = run(
            &s,
            "python3 -c 'import socket
c=socket.socket(); c.settimeout(5); c.connect((\"127.0.0.1\",18080)); print(c.recv(64).decode())'",
        )
        .await;
        out = text;
        if code == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        out.contains("hello-from-process"),
        "a later command must reach the background process over loopback: {out}"
    );

    s.stop().await;
}

/// Sharing a network namespace must not mean sharing a PID namespace: each command
/// gets its own, so one cannot see or signal another's processes, and killing a
/// command reaps exactly its own tree.
#[tokio::test]
async fn commands_share_the_network_namespace_but_not_the_pid_namespace() {
    skip_if_unsupported!();
    let p = Project::new();
    let s = sandbox(&p.path());

    // Two commands in the same session; each should be PID 1-ish in its own view.
    let (_, first) = run(
        &s,
        "python3 -c 'import os; print(\"pids\", sorted(int(d) for d in os.listdir(\"/proc\") if d.isdigit()))'",
    )
    .await;
    let (_, second) = run(
        &s,
        "python3 -c 'import os; print(\"pids\", sorted(int(d) for d in os.listdir(\"/proc\") if d.isdigit()))'",
    )
    .await;
    for out in [&first, &second] {
        assert!(out.contains("pids ["), "{out}");
        // A private PID namespace shows only a handful of processes; the host's
        // would show hundreds.
        let count = out.matches(',').count();
        assert!(
            count < 8,
            "each command must have its own PID namespace, saw: {out}"
        );
    }

    s.stop().await;
}

/// A grant approved between two commands takes effect for the second and not the
/// first — the whole point of rebuilding the plan per command, and the thing a
/// container's fixed mounts cannot do.
#[tokio::test]
async fn a_grant_applies_to_the_next_command_but_not_the_current_one() {
    skip_if_unsupported!();
    let p = Project::new();
    let s = sandbox(&p.path());

    // A directory outside the project, with a marker file.
    let outside = assert_fs::TempDir::new().unwrap();
    let outside_path = std::fs::canonicalize(outside.path()).unwrap();
    std::fs::write(outside_path.join("marker.txt"), "granted-content").unwrap();
    let probe = format!("cat {}/marker.txt 2>&1", outside_path.display());

    let (_, before) = run(&s, &probe).await;
    assert!(
        !before.contains("granted-content"),
        "the path must not be reachable before it is granted: {before}"
    );

    s.add_grant(
        outside_path.clone(),
        true,
        cowboy_cli::sandbox::grants::Persistence::Session,
    )
    .unwrap();

    let (code, after) = run(&s, &probe).await;
    assert_eq!(code, 0, "{after}");
    assert!(
        after.contains("granted-content"),
        "the grant must apply to the next command with no restart: {after}"
    );

    s.stop().await;
}

/// Scratch space survives from one command to the next. Each command gets a fresh
/// mount namespace, so `/tmp` was private per command: an agent that wrote a file
/// in one shell found it gone in the next, which broke the ordinary
/// `curl -o /tmp/x` then `read /tmp/x` pattern with a bare "No such file".
///
/// The three writable scratch paths are checked together because a shell that can
/// only keep `/tmp` still surprises anything using `TMPDIR=/var/tmp` or a pid file
/// under `/run`.
#[tokio::test]
async fn scratch_space_survives_between_commands_and_is_gone_after_the_session() {
    skip_if_unsupported!();
    let p = Project::new();
    let s = sandbox(&p.path());

    for dir in ["/tmp", "/run", "/var/tmp"] {
        let (code, out) = run(&s, &format!("echo kept > {dir}/marker 2>&1")).await;
        assert_eq!(code, 0, "{dir} must be writable: {out}");
    }
    for dir in ["/tmp", "/run", "/var/tmp"] {
        let (code, out) = run(&s, &format!("cat {dir}/marker 2>&1")).await;
        assert_eq!(code, 0, "{dir}/marker must still exist: {out}");
        assert!(out.contains("kept"), "{dir}/marker lost its content: {out}");
    }

    // …and it is genuinely scratch: ending the session discards it, so the next
    // session does not inherit whatever the last one left lying around.
    s.stop().await;
    let s2 = sandbox(&p.path());
    let (_, out) = run(&s2, "cat /tmp/marker 2>&1").await;
    assert!(
        !out.contains("kept"),
        "a new session must start with empty scratch: {out}"
    );
    s2.stop().await;
}

/// Two sandboxes on the *same project* are two independent sessions — each starts its
/// own holder and so its own namespaces — so they must not share scratch. Before this
/// was keyed to the owning process they did, which meant `cowboy sandbox exec` run
/// alongside a live agent session both saw its `/tmp` and deleted it on the way out.
#[test]
fn scratch_is_scoped_to_the_owning_process_not_the_project() {
    let key = cowboy_cli::project::scratch_key("cowboy-deadbeef");
    assert_eq!(key, format!("cowboy-deadbeef.{}", std::process::id()));

    // Abandoned scratch is reaped rather than accumulating: a process killed with
    // SIGKILL never runs its own cleanup, and scratch is disk-backed.
    let mine = cowboy_cli::project::ensure_scratch_dir(&key).unwrap();
    let base = mine.parent().unwrap().to_path_buf();
    // pid 0 is never a live process, so this stands in for a crashed owner.
    let abandoned = base.join("cowboy-deadbeef.0");
    std::fs::create_dir_all(abandoned.join("tmp")).unwrap();
    cowboy_cli::project::ensure_scratch_dir(&key).unwrap();
    assert!(!abandoned.exists(), "a dead owner's scratch must be reaped");
    assert!(mine.exists(), "the live owner's scratch must be kept");

    cowboy_cli::project::remove_scratch_dir(&key);
    assert!(!mine.exists());
}

/// A running process cannot see a later grant, and the user is told so rather than
/// left to debug a dev server that cannot read a folder every new command can.
#[tokio::test]
async fn a_running_process_is_reported_as_stale_after_a_grant() {
    skip_if_unsupported!();
    let p = Project::new();
    let mut s = sandbox(&p.path());
    let mut notices = s.status_channel();

    let def = ProcessDef {
        command: "sleep 60".to_string(),
        cwd: "/workspace".to_string(),
        auto_start: false,
    };
    s.start_process("web", &def).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let outside = assert_fs::TempDir::new().unwrap();
    s.add_grant(
        std::fs::canonicalize(outside.path()).unwrap(),
        true,
        cowboy_cli::sandbox::grants::Persistence::Session,
    )
    .unwrap();

    let mut found = None;
    while let Ok(msg) = notices.try_recv() {
        if msg.contains("web") {
            found = Some(msg);
        }
    }
    let msg = found.expect("a notice naming the stale process");
    assert!(
        msg.contains("Restart"),
        "the notice must say what to do: {msg}"
    );
    assert_eq!(s.stale_processes(), vec!["web".to_string()]);

    s.stop().await;
}

/// Stopping the session must not leave background processes behind.
#[tokio::test]
async fn stopping_the_session_reaps_background_processes() {
    skip_if_unsupported!();
    let p = Project::new();
    let s = sandbox(&p.path());
    let marker = format!("cowboy-session-probe-{}", std::process::id());

    let def = ProcessDef {
        command: format!("exec -a {marker} sleep 120"),
        cwd: "/workspace".to_string(),
        auto_start: false,
    };
    s.start_process("web", &def).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        processes_matching(&marker) > 0,
        "the background process should be running"
    );

    s.stop().await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        processes_matching(&marker),
        0,
        "stopping the session must reap its background processes"
    );
}

/// Reading `/proc` directly rather than `pgrep -f`, whose pattern also matches the
/// shell that invoked it.
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

/// The `cowboy grant` story: a grant recorded in the store by *another process* is
/// picked up by a session that is already running, with no message sent to it.
///
/// This is why the plan is rebuilt per command rather than cached — it is what makes
/// `cowboy grant ~/other-repo` in a second terminal work on the very next command
/// instead of needing a restart or a control channel to the worker.
#[tokio::test]
async fn a_grant_written_by_another_process_reaches_a_running_session() {
    skip_if_unsupported!();
    let p = Project::new();
    let (s, store) = sandbox_with_store(&p.path());

    let outside = assert_fs::TempDir::new().unwrap();
    let outside_path = std::fs::canonicalize(outside.path()).unwrap();
    std::fs::write(outside_path.join("marker.txt"), "granted-out-of-band").unwrap();
    let probe = format!("cat {}/marker.txt 2>&1", outside_path.display());

    // Bring the session up and confirm the path is not reachable yet, so the session
    // is definitely already running when the grant appears.
    let (_, before) = run(&s, &probe).await;
    assert!(
        !before.contains("granted-out-of-band"),
        "not reachable before the grant: {before}"
    );

    // Exactly what `cowboy grant <path> --ro` writes.
    cowboy_cli::sandbox::grants::add_in(
        store.path(),
        &p.path(),
        &cowboy_sandbox::plan::Grant {
            path: outside_path.clone(),
            read_only: true,
        },
        cowboy_cli::sandbox::grants::Persistence::Project,
    )
    .unwrap();

    let (code, after) = run(&s, &probe).await;
    assert_eq!(code, 0, "{after}");
    assert!(
        after.contains("granted-out-of-band"),
        "a grant recorded out of band must reach the next command of a running \
         session: {after}"
    );
    s.stop().await;
}

/// A saved grant naming a credential store is refused when it is *used*, not merely
/// when it was written — the file is host-owned but hand-editable, and a global grant
/// outlives the project it was made in. Asserted against a real sandbox: the path
/// must not be readable inside it.
///
/// The home directory is faked so this creates its own credential store on disk
/// rather than writing into the developer's real `~/.aws`, and so it asserts the same
/// thing on a machine that has no AWS config at all.
#[tokio::test]
async fn a_saved_grant_for_credentials_is_refused_by_a_real_sandbox() {
    skip_if_unsupported!();
    let p = Project::new();
    let home = assert_fs::TempDir::new().unwrap();
    let home_path = std::fs::canonicalize(home.path()).unwrap();
    let aws = home_path.join(".aws");
    std::fs::create_dir_all(&aws).unwrap();
    let marker = aws.join("credentials");
    std::fs::write(&marker, "secret-material").unwrap();

    let (s, store) = sandbox_with_probe(&p.path(), Host::with_home(home_path));

    cowboy_cli::sandbox::grants::add_in(
        store.path(),
        &p.path(),
        &cowboy_sandbox::plan::Grant {
            path: aws.clone(),
            read_only: true,
        },
        cowboy_cli::sandbox::grants::Persistence::Global,
    )
    .unwrap();

    // The grant is definitely on record — so a pass here is the denylist refusing it,
    // not the grant having failed to be saved.
    assert!(
        !cowboy_cli::sandbox::grants::load_in(store.path(), &p.path()).is_empty(),
        "the test must actually have recorded a grant"
    );

    let (_, out) = run(&s, &format!("cat {} 2>&1", marker.display())).await;
    s.stop().await;

    assert!(
        !out.contains("secret-material"),
        "a saved grant must not be able to expose a credential store: {out}"
    );
}

/// A memory ceiling is real: a command that allocates past it is killed, and the
/// machine survives. This is the capability the container runtime provided for free
/// and that had to be rebuilt — without it one runaway build takes the box down.
///
/// Uses the cgroup's own OOM killer, so only the offending process dies; nothing
/// outside the sandbox is affected.
#[tokio::test]
async fn a_memory_ceiling_kills_a_runaway_command_and_not_the_machine() {
    skip_if_unsupported!();
    if skip_no_cgroups() {
        return;
    }
    let p = Project::new();
    let security = SecurityConfig {
        sandbox: cowboy_core::config::SandboxConfig {
            memory: Some("128m".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let (s, _store) = sandbox_with(&p.path(), Host::real(), security);

    // A modest allocation first: the ceiling must not break ordinary work.
    let (code, out) = run(
        &s,
        "python3 -c 'b = bytearray(16 * 1024 * 1024); print(\"ALLOCATED\", len(b))'",
    )
    .await;
    assert_eq!(code, 0, "a small allocation must still succeed: {out}");
    assert!(out.contains("ALLOCATED"), "{out}");

    // Then past the ceiling, touching every page so it is charged to the cgroup
    // rather than merely reserved.
    let (code, out) = run(
        &s,
        "python3 -c 'b = bytearray(1024 * 1024 * 1024); b[::4096] = b\"x\" * (len(b)//4096); \
         print(\"LEAKED\", len(b))' 2>&1",
    )
    .await;
    assert!(
        !out.contains("LEAKED"),
        "an allocation past the ceiling must not succeed: {out}"
    );
    assert_ne!(code, 0, "the runaway command must fail: {out}");

    // And the session is still usable afterwards — the limit bounded one command,
    // it did not tear the sandbox down.
    let (code, out) = run(&s, "echo STILL-ALIVE").await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("STILL-ALIVE"), "{out}");
    s.stop().await;
}

/// The process ceiling bounds a fork bomb. `pids.max` is charged to the session's
/// cgroup, so the refusal lands inside the sandbox and the host's process table is
/// never at risk.
#[tokio::test]
async fn a_fork_bomb_is_bounded_by_the_process_ceiling() {
    skip_if_unsupported!();
    if skip_no_cgroups() {
        return;
    }
    let p = Project::new();
    let (s, _store) = sandbox_with_store(&p.path());

    // The default ceiling is 4096, so this asks for more than that and expects to be
    // refused rather than to succeed 20000 times.
    let (_, out) = run(
        &s,
        r#"python3 -c '
import os
n = 0
try:
    while n < 20000:
        if os.fork() == 0:
            os._exit(0)
        n += 1
except OSError:
    print("FORK REFUSED after", n)
else:
    print("NO LIMIT", n)
' 2>&1"#,
    )
    .await;
    assert!(
        out.contains("FORK REFUSED"),
        "the process ceiling must stop a fork bomb: {out}"
    );
    s.stop().await;
}

/// The session's cgroup is reaped on teardown; a long-lived daemon must not leak a
/// directory per session.
#[tokio::test]
async fn the_session_cgroup_is_reaped_on_teardown() {
    skip_if_unsupported!();
    if skip_no_cgroups() {
        return;
    }
    let p = Project::new();
    let (s, _store) = sandbox_with_store(&p.path());
    let (code, _) = run(&s, "true").await;
    assert_eq!(code, 0);

    // Find it by name, the way an operator would.
    let name = s.cgroup_name().to_string();
    let found = |name: &str| -> Vec<std::path::PathBuf> {
        let out = std::process::Command::new("find")
            .args([
                "/sys/fs/cgroup",
                "-maxdepth",
                "6",
                "-type",
                "d",
                "-name",
                &format!("cowboy-{name}"),
            ])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        out.lines().map(std::path::PathBuf::from).collect()
    };
    assert!(
        !found(&name).is_empty(),
        "the session's cgroup should exist while it runs"
    );

    s.stop().await;
    // The holder's exit is what empties it; give the kernel a moment.
    for _ in 0..20 {
        if found(&name).is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the session cgroup was not reaped: {:?}", found(&name));
}

/// Regression: one session tearing down must not brick a live sibling.
///
/// The cgroup used to be named from the project, so every session in a project shared
/// one directory (`Cgroup::create` reuses an existing one). `stop()` then removed it
/// out from under the survivors — `remove_dir` succeeds whenever the cgroup holds no
/// processes, which is true of any sibling idle *between* commands. Joining the cgroup
/// is fatal on purpose, so every later command failed with `spawning the sandbox: No
/// such file or directory` and the session was alive but unable to run anything.
///
/// Observed for real: a foreman that spawned four subagents could not run a single
/// shell command after they finished.
#[tokio::test]
async fn a_sibling_teardown_leaves_a_live_session_working() {
    skip_if_unsupported!();
    if skip_no_cgroups() {
        return;
    }
    let p = Project::new();
    let (a, _sa) = sandbox_with_store(&p.path());
    let (b, _sb) = sandbox_with_store(&p.path());

    let (code, out) = run(&a, "echo A-first").await;
    assert_eq!(code, 0, "{out}");

    // B runs and stops while A sits idle between commands — the foreman/subagent
    // shape, and the state in which the shared cgroup was removable.
    let (code, out) = run(&b, "echo B-only").await;
    assert_eq!(code, 0, "{out}");
    b.stop().await;

    let (res, out) = a
        .run_capture("echo A-second", None, 120)
        .await
        .expect("A must still be able to run a command after its sibling stopped");
    a.stop().await;
    assert_eq!(res.exit_code, 0, "{out}");
    assert!(out.contains("A-second"), "{out}");
}

/// And the limits are per-session, not a project-wide ceiling shared out between
/// however many sessions are live. Sharing one cgroup silently divided the documented
/// per-session ceiling by the number of siblings.
#[tokio::test]
async fn concurrent_sessions_get_independent_cgroups() {
    skip_if_unsupported!();
    if skip_no_cgroups() {
        return;
    }
    let p = Project::new();
    let (a, _sa) = sandbox_with_store(&p.path());
    let (b, _sb) = sandbox_with_store(&p.path());
    assert_ne!(
        a.cgroup_name(),
        b.cgroup_name(),
        "two sessions in one project must not name the same cgroup"
    );

    // Bring both up, then assert each has its own directory in the kernel.
    for s in [&a, &b] {
        let (code, out) = run(s, "true").await;
        assert_eq!(code, 0, "{out}");
    }
    let dirs = |name: &str| -> Vec<PathBuf> {
        let out = std::process::Command::new("find")
            .args([
                "/sys/fs/cgroup",
                "-maxdepth",
                "6",
                "-type",
                "d",
                "-name",
                &format!("cowboy-{name}"),
            ])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        out.lines().map(PathBuf::from).collect()
    };
    let (da, db) = (dirs(a.cgroup_name()), dirs(b.cgroup_name()));
    assert!(!da.is_empty(), "session A has no cgroup");
    assert!(!db.is_empty(), "session B has no cgroup");
    assert!(
        da.iter().all(|x| !db.contains(x)),
        "the two sessions share a cgroup directory: {da:?} vs {db:?}"
    );
    a.stop().await;
    b.stop().await;
}
