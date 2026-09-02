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
    let inputs = PlanInputs {
        root,
        security: &sec,
        grants: &[],
        mask_file: &mask,
        relay_port: 8443,
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

#[tokio::test]
async fn the_host_home_directory_is_not_visible() {
    skip_if_unsupported!();
    let p = Project::new();
    let home = cowboy_core::config::expand_path("~").unwrap();
    let (_code, out) = run(
        &p.path(),
        &format!("ls {} 2>&1 || true", home.display()),
        60,
    )
    .await;
    assert!(
        out.contains("No such file") || out.contains("cannot access"),
        "the host home directory must not be reachable: {out}"
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
