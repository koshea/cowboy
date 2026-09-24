//! The macOS boundary, end to end: real Seatbelt profiles, the real proxy, real
//! processes.
//!
//! The Linux suites (`sandbox_exec.rs`, `sandbox_session.rs`, `sandbox_egress.rs`)
//! assert the same properties against namespaces; this is their counterpart. Each
//! denial is paired with an allowance of the same kind — reading a masked file fails
//! *and* reading its neighbour works — because a sandbox that denies everything
//! passes every denial test while being useless, and a test that only checks one
//! direction cannot tell the two apart.
//!
//! Self-skips when the sandbox cannot run (another OS, no `cowboy` binary built);
//! `COWBOY_SANDBOX_TESTS=required` turns a skip into a failure.

#![cfg(target_os = "macos")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cowboy_cli::sandbox::native::NativeSandbox;
use cowboy_cli::sandbox::Sandbox;
use cowboy_core::config::SecurityConfig;
use cowboy_core::netproto::{NetworkAttempt, Verdict};
use cowboy_sandbox::HostProbe;

/// The real host, optionally with a faked home directory, so a test can put a
/// credential store on disk without touching the developer's own `~`.
struct Host {
    home: Option<PathBuf>,
}

impl HostProbe for Host {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn git_common_dir(&self, _root: &Path) -> Option<PathBuf> {
        None
    }
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
        cowboy_exe()
    }
    fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok()
    }
    fn darwin_user_temp(&self) -> Option<PathBuf> {
        Some(std::env::temp_dir())
    }
}

/// The `cowboy` binary built alongside this test.
fn cowboy_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe()
        .ok()?
        .parent()?
        .parent()?
        .join("cowboy");
    exe.exists().then_some(exe)
}

macro_rules! skip_if_unsupported {
    () => {
        if cowboy_exe().is_none() {
            let why = "the cowboy binary is not built alongside the test";
            if std::env::var("COWBOY_SANDBOX_TESTS").as_deref() == Ok("required") {
                panic!("sandbox tests required but unsupported here: {why}");
            }
            eprintln!("skipping: {why}");
            return;
        }
    };
}

fn online() -> bool {
    std::net::TcpStream::connect_timeout(&"1.1.1.1:443".parse().unwrap(), Duration::from_secs(4))
        .is_ok()
}

macro_rules! skip_if_offline {
    () => {
        if !online() {
            eprintln!("skipping: this machine has no internet access");
            return;
        }
    };
}

/// Allows every `ask`: for tests of the path a user's approval opens, as distinct
/// from the policy that decides whether to ask.
struct AllowAll;

#[async_trait::async_trait]
impl cowboy_gateway::Approver for AllowAll {
    async fn ask(&self, _a: &NetworkAttempt, _r: Option<&str>) -> Verdict {
        Verdict::Allow
    }
    async fn event(&self, _a: &NetworkAttempt, _v: Verdict, _r: String) {}
}

struct Project {
    dir: assert_fs::TempDir,
}

impl Project {
    fn new() -> Self {
        let dir = assert_fs::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".cowboy")).unwrap();
        std::fs::write(
            dir.path().join(".cowboy/security.yaml"),
            "version: 1\n# secret\n",
        )
        .unwrap();
        Self { dir }
    }
    fn path(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap()
    }
}

struct Fixture {
    sandbox: NativeSandbox,
    project: Project,
    _store: assert_fs::TempDir,
}

fn fixture_with(
    security: SecurityConfig,
    home: Option<PathBuf>,
    approver: Arc<dyn cowboy_gateway::Approver>,
) -> Fixture {
    let project = Project::new();
    let store = assert_fs::TempDir::new().unwrap();
    let sandbox = NativeSandbox::new(project.path(), security, Box::new(Host { home }), approver)
        .unwrap()
        .with_grants_dir(store.path().to_path_buf());
    Fixture {
        sandbox,
        project,
        _store: store,
    }
}

fn fixture() -> Fixture {
    fixture_with(
        SecurityConfig::default(),
        None,
        Arc::new(cowboy_gateway::DenyAll),
    )
}

async fn run(s: &NativeSandbox, command: &str) -> (i32, String) {
    let (res, out) = s.run_capture(command, None, 120).await.unwrap();
    (res.exit_code, out)
}

/// A one-line HTTP server on the host (outside any sandbox), for loopback tests.
fn host_http_server(body: &'static str) -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        listener.set_nonblocking(false).expect("blocking listener");
        // Serve until the test process ends; each test only needs a request or two.
        for conn in listener.incoming().flatten().take(4) {
            let mut conn = conn;
            let mut buf = [0u8; 1024];
            let _ = conn.read(&mut buf);
            let _ = write!(
                conn,
                "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (port, handle)
}

// ---- filesystem -------------------------------------------------------------------

#[tokio::test]
async fn a_command_runs_in_the_project_at_its_host_path() {
    skip_if_unsupported!();
    let fx = fixture();
    let (code, out) = run(&fx.sandbox, "pwd; echo ok > made-here && cat made-here").await;
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains(&fx.project.path().display().to_string()),
        "{out}"
    );
    assert!(out.contains("ok"), "{out}");
    assert!(fx.project.path().join("made-here").exists());
    fx.sandbox.stop().await;
}

/// The mask must hold for reads *and* writes, and must not take the directory with
/// it: the rest of `.cowboy/` stays usable.
#[tokio::test]
async fn host_owned_config_is_masked_and_its_neighbours_are_not() {
    skip_if_unsupported!();
    let fx = fixture();
    let (code, out) = run(&fx.sandbox, "cat .cowboy/security.yaml").await;
    assert_ne!(code, 0, "the mask must refuse the read: {out}");
    assert!(!out.contains("secret"), "{out}");

    let (code, out) = run(&fx.sandbox, "echo 'version: 99' > .cowboy/security.yaml").await;
    assert_ne!(code, 0, "the mask must refuse the write: {out}");
    let (code, out) = run(&fx.sandbox, "echo x > .cowboy/models.yaml").await;
    assert_ne!(
        code, 0,
        "a masked file that does not exist cannot be created: {out}"
    );
    assert!(!fx.project.path().join(".cowboy/models.yaml").exists());

    let (code, out) = run(
        &fx.sandbox,
        "echo fine > .cowboy/notes && cat .cowboy/notes",
    )
    .await;
    assert_eq!(code, 0, "{out}");
    assert!(
        std::fs::read_to_string(fx.project.path().join(".cowboy/security.yaml"))
            .unwrap()
            .contains("secret"),
        "the host's copy is untouched"
    );
    fx.sandbox.stop().await;
}

/// Home is not browsable, and a credential in it does not even appear to exist —
/// while the exposed tool directories in the same home do.
#[tokio::test]
async fn home_is_not_browsable_and_credentials_do_not_appear_to_exist() {
    skip_if_unsupported!();
    let home = assert_fs::TempDir::new().unwrap();
    let home_path = std::fs::canonicalize(home.path()).unwrap();
    std::fs::create_dir_all(home_path.join(".ssh")).unwrap();
    std::fs::write(home_path.join(".ssh/id_ed25519"), "PRIVATE").unwrap();
    std::fs::create_dir_all(home_path.join(".cargo/bin")).unwrap();
    std::fs::write(
        home_path.join(".cargo/bin/tool"),
        "#!/bin/sh\necho tool-ran\n",
    )
    .unwrap();
    let fx = fixture_with(
        SecurityConfig::default(),
        Some(home_path.clone()),
        Arc::new(cowboy_gateway::DenyAll),
    );
    let h = home_path.display();

    let (code, out) = run(&fx.sandbox, &format!("ls '{h}'")).await;
    assert_ne!(code, 0, "home must not be listable: {out}");
    let (code, out) = run(&fx.sandbox, &format!("cat '{h}/.ssh/id_ed25519'")).await;
    assert_ne!(code, 0, "{out}");
    assert!(!out.contains("PRIVATE"), "{out}");
    let (_, out) = run(
        &fx.sandbox,
        &format!("test -e '{h}/.ssh' && echo visible || echo hidden"),
    )
    .await;
    assert!(
        out.contains("hidden"),
        "a credential store must not be stat-able: {out}"
    );

    let (code, out) = run(&fx.sandbox, &format!("sh '{h}/.cargo/bin/tool'")).await;
    assert_eq!(code, 0, "an exposed tool directory is readable: {out}");
    assert!(out.contains("tool-ran"), "{out}");
    fx.sandbox.stop().await;
}

#[tokio::test]
async fn writes_land_only_in_the_project_scratch_and_home() {
    skip_if_unsupported!();
    let fx = fixture();
    let outside = assert_fs::TempDir::new().unwrap();
    let outside = std::fs::canonicalize(outside.path()).unwrap();
    let (code, out) = run(
        &fx.sandbox,
        &format!("echo x > '{}/nope'", outside.display()),
    )
    .await;
    assert_ne!(code, 0, "{out}");
    assert!(!outside.join("nope").exists());

    let (code, out) = run(&fx.sandbox, "echo x > /tmp/cowboy-sandbox-test-nope").await;
    assert_ne!(
        code, 0,
        "the host's /tmp is shared with everything; not ours: {out}"
    );

    let (code, out) = run(
        &fx.sandbox,
        "echo a > \"$TMPDIR/a\" && echo b > \"$HOME/b\" && cat \"$TMPDIR/a\" \"$HOME/b\"",
    )
    .await;
    assert_eq!(code, 0, "{out}");
    fx.sandbox.stop().await;
}

/// The host's `xcrun` trusts its lookup cache to say where `git` and `clang` are;
/// a command that could write it would choose what the user's next `git` runs. The
/// shims must still work off the host's copy.
#[tokio::test]
async fn the_hosts_xcrun_cache_cannot_be_planted() {
    skip_if_unsupported!();
    let fx = fixture();
    let (code, out) = run(
        &fx.sandbox,
        "touch \"$(getconf DARWIN_USER_TEMP_DIR)xcrun_db-cowboy-planted\"",
    )
    .await;
    assert_ne!(code, 0, "{out}");
    assert!(!std::env::temp_dir()
        .join("xcrun_db-cowboy-planted")
        .exists());

    let (code, out) = run(&fx.sandbox, "/usr/bin/git --version").await;
    assert_eq!(code, 0, "the developer shims still run: {out}");
    fx.sandbox.stop().await;
}

#[tokio::test]
async fn scratch_survives_between_commands() {
    skip_if_unsupported!();
    let fx = fixture();
    let (code, out) = run(&fx.sandbox, "echo kept > \"$TMPDIR/k\"").await;
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&fx.sandbox, "cat \"$TMPDIR/k\"").await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("kept"), "{out}");
    fx.sandbox.stop().await;
}

#[tokio::test]
async fn a_runtime_grant_reaches_the_next_command() {
    skip_if_unsupported!();
    let fx = fixture();
    let other = assert_fs::TempDir::new().unwrap();
    let other = std::fs::canonicalize(other.path()).unwrap();
    let cmd = format!("echo g > '{}/granted'", other.display());
    let (code, _) = run(&fx.sandbox, &cmd).await;
    assert_ne!(code, 0, "not granted yet");
    fx.sandbox
        .add_grant(
            other.clone(),
            false,
            cowboy_cli::sandbox::grants::Persistence::Session,
        )
        .unwrap();
    let (code, out) = run(&fx.sandbox, &cmd).await;
    assert_eq!(code, 0, "{out}");
    assert!(other.join("granted").exists());
    fx.sandbox.stop().await;
}

/// The structured file tools run the cowboy binary inside the sandbox too; on macOS
/// it is at its host path rather than a bind target.
#[tokio::test]
async fn the_file_tools_work_inside_the_sandbox() {
    skip_if_unsupported!();
    let fx = fixture();
    let (res, out) = fx
        .sandbox
        .fileop(r#"{"op":"write","path":"notes.txt","content":"one\ntwo\n"}"#)
        .await
        .unwrap();
    assert_eq!(res.exit_code, 0, "{out}");
    let (res, out) = fx
        .sandbox
        .fileop(r#"{"op":"read","path":"notes.txt"}"#)
        .await
        .unwrap();
    assert_eq!(res.exit_code, 0, "{out}");
    assert!(out.contains("two"), "{out}");
    let (res, out) = fx
        .sandbox
        .fileop(r#"{"op":"read","path":".cowboy/security.yaml"}"#)
        .await
        .unwrap();
    assert!(
        res.exit_code != 0 || !out.contains("secret"),
        "the mask applies to the file tools too: {out}"
    );
    fx.sandbox.stop().await;
}

// ---- network ----------------------------------------------------------------------

/// **A data transfer, not a connect** — and only meaningful online, where the same
/// connection from the host would succeed.
#[tokio::test]
async fn a_direct_connection_out_is_refused() {
    skip_if_unsupported!();
    skip_if_offline!();
    let fx = fixture();
    let (code, out) = run(
        &fx.sandbox,
        "/usr/bin/python3 -c 'import socket; s=socket.create_connection((\"1.1.1.1\",443),5); \
         s.sendall(b\"x\"); print(\"CONNECTED\")'",
    )
    .await;
    assert_ne!(code, 0, "{out}");
    assert!(!out.contains("CONNECTED"), "{out}");
    fx.sandbox.stop().await;
}

#[tokio::test]
async fn dns_is_unavailable_inside_the_sandbox() {
    skip_if_unsupported!();
    skip_if_offline!();
    let fx = fixture();
    let (code, out) = run(
        &fx.sandbox,
        "/usr/bin/python3 -c 'import socket; print(socket.getaddrinfo(\"github.com\",443))'",
    )
    .await;
    assert_ne!(code, 0, "{out}");
    fx.sandbox.stop().await;
}

/// The proxy path: an allowed domain works, one the policy would ask about is
/// refused with nobody to ask, and the refusal is visible to the command.
#[tokio::test]
async fn the_proxy_enforces_the_policy() {
    skip_if_unsupported!();
    skip_if_offline!();
    let fx = fixture();
    let (code, out) = run(
        &fx.sandbox,
        "curl -sS -m 20 -o /dev/null -w '%{http_code}' https://github.com",
    )
    .await;
    assert_eq!(code, 0, "an allowlisted domain must work: {out}");
    assert!(out.starts_with('2') || out.starts_with('3'), "{out}");

    let (code, out) = run(&fx.sandbox, "curl -sS -m 20 https://example.com").await;
    assert_ne!(code, 0, "{out}");
    assert!(
        out.contains("403"),
        "the refusal is the proxy's, not a timeout: {out}"
    );
    fx.sandbox.stop().await;
}

/// Decided before anything is dialled, so this holds offline too.
#[tokio::test]
async fn the_metadata_address_is_refused() {
    skip_if_unsupported!();
    let fx = fixture();
    let (_, out) = run(
        &fx.sandbox,
        "curl -sS -m 10 -o /dev/null -w '%{http_code}' http://169.254.169.254/latest/meta-data/",
    )
    .await;
    assert!(out.contains("403"), "{out}");
    fx.sandbox.stop().await;
}

/// The proxy's port is reachable by anything on the machine, so credentials are the
/// control: a wrong token is refused, and a command's token dies with it.
#[tokio::test]
async fn proxy_credentials_are_per_command() {
    skip_if_unsupported!();
    let fx = fixture();
    // Keep this command's proxy URL for the next one to try.
    let (code, out) = run(&fx.sandbox, "echo \"$HTTPS_PROXY\" > \"$TMPDIR/old-proxy\"").await;
    assert_eq!(code, 0, "{out}");

    let (_, out) = run(
        &fx.sandbox,
        "curl -sS -m 10 -x \"$(cat \"$TMPDIR/old-proxy\")\" -o /dev/null -w '%{http_code}' \
         http://169.254.169.254/",
    )
    .await;
    assert!(
        out.contains("407"),
        "a finished command's credentials are revoked: {out}"
    );

    let (_, out) = run(
        &fx.sandbox,
        "u=$(echo \"$HTTPS_PROXY\" | sed 's#//[^@]*@#//c9999:wrong@#'); \
         curl -sS -m 10 -x \"$u\" -o /dev/null -w '%{http_code}' http://169.254.169.254/",
    )
    .await;
    assert!(out.contains("407"), "{out}");

    let (_, out) = run(
        &fx.sandbox,
        "curl -sS -m 10 -o /dev/null -w '%{http_code}' http://169.254.169.254/",
    )
    .await;
    assert!(
        out.contains("403"),
        "and the command's own credentials get as far as the policy: {out}"
    );
    fx.sandbox.stop().await;
}

/// Loopback is the host's, so a host service is not reachable directly — but is
/// through the proxy once approved, and directly when its port is configured.
#[tokio::test]
async fn loopback_reaches_host_services_only_through_policy() {
    skip_if_unsupported!();
    let (port, _server) = host_http_server("host-service");
    let direct = format!(
        "/usr/bin/python3 -c 'import socket; s=socket.create_connection((\"127.0.0.1\",{port}),5); \
         s.sendall(b\"GET / HTTP/1.0\\r\\n\\r\\n\"); \
         print(b\"\".join(iter(lambda: s.recv(100), b\"\")))'"
    );

    let fx = fixture();
    let (code, out) = run(&fx.sandbox, &direct).await;
    assert_ne!(
        code, 0,
        "a direct loopback connection must be refused: {out}"
    );
    let (_, out) = run(
        &fx.sandbox,
        &format!("curl -sS -m 10 http://127.0.0.1:{port}/"),
    )
    .await;
    assert!(
        !out.contains("host-service"),
        "`ask` with nobody to ask is a no: {out}"
    );
    fx.sandbox.stop().await;

    // Approved: the proxy dials it.
    let fx = fixture_with(SecurityConfig::default(), None, Arc::new(AllowAll));
    let (code, out) = run(
        &fx.sandbox,
        &format!("curl -sS -m 10 http://127.0.0.1:{port}/"),
    )
    .await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("host-service"), "{out}");
    fx.sandbox.stop().await;

    // Configured: the profile admits the port directly.
    let mut security = SecurityConfig::default();
    security.sandbox.loopback_ports = vec![port];
    let fx = fixture_with(security, None, Arc::new(cowboy_gateway::DenyAll));
    let (code, out) = run(&fx.sandbox, &direct).await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("host-service"), "{out}");
    fx.sandbox.stop().await;
}

/// The agent's own servers: it can bind loopback, and talk to itself over a unix
/// socket in its own directories — never the host's.
#[tokio::test]
async fn the_agent_can_serve_on_loopback_and_its_own_sockets() {
    skip_if_unsupported!();
    let fx = fixture();
    let (code, out) = run(
        &fx.sandbox,
        "/usr/bin/python3 -c '
import os, socket
t = socket.socket(); t.bind((\"127.0.0.1\", 0)); t.listen(1); print(\"BOUND\")
p = os.environ[\"TMPDIR\"] + \"/s.sock\"
s = socket.socket(socket.AF_UNIX); s.bind(p); s.listen(1)
c = socket.socket(socket.AF_UNIX); c.connect(p); c.sendall(b\"hi\")
a, _ = s.accept(); print(\"UNIX\", a.recv(2))
try:
    h = socket.socket(socket.AF_UNIX); h.connect(\"/var/run/mDNSResponder\"); print(\"HOST-SOCKET\")
except OSError as e:
    print(\"refused\", e.errno)
'",
    )
    .await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("BOUND") && out.contains("UNIX b'hi'"), "{out}");
    assert!(!out.contains("HOST-SOCKET"), "{out}");
    fx.sandbox.stop().await;
}

// ---- processes --------------------------------------------------------------------

/// Each of these would get code run by a process that was never confined.
#[tokio::test]
async fn escape_routes_to_unconfined_processes_are_closed() {
    skip_if_unsupported!();
    let fx = fixture();
    let marker = std::env::temp_dir().join(format!("cowboy-escape-{}", std::process::id()));
    let m = marker.display();
    for (what, cmd) in [
        ("LaunchServices", "open -g -a TextEdit".to_string()),
        (
            "AppleEvents",
            format!("osascript -e 'do shell script \"touch {m}\"'"),
        ),
        (
            "launchd",
            format!("launchctl submit -l cowboy.escape.test -- /usr/bin/touch {m}"),
        ),
        ("at", format!("echo 'touch {m}' | at now")),
    ] {
        let (code, out) = run(&fx.sandbox, &cmd).await;
        assert_ne!(code, 0, "{what} must be refused: {out}");
    }
    let _ = std::process::Command::new("launchctl")
        .args(["remove", "cowboy.escape.test"])
        .status();
    std::thread::sleep(Duration::from_secs(1));
    assert!(!marker.exists(), "something ran outside the sandbox");

    // A process outside the sandbox cannot be signalled, or seen.
    let me = std::process::id();
    let (code, out) = run(&fx.sandbox, &format!("kill -0 {me}")).await;
    assert_ne!(code, 0, "{out}");
    fx.sandbox.stop().await;
}

#[tokio::test]
async fn a_background_job_does_not_outlive_its_command() {
    skip_if_unsupported!();
    let fx = fixture();
    let (code, out) = run(&fx.sandbox, "sleep 300 & echo $! > \"$TMPDIR/bg\"").await;
    assert_eq!(code, 0, "{out}");
    let (_, out) = run(&fx.sandbox, "cat \"$TMPDIR/bg\"").await;
    let pid: u32 = out.trim().parse().expect("the job's pid");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !alive(pid),
        "the backgrounded job ({pid}) survived its command"
    );
    fx.sandbox.stop().await;
}

/// No PID namespace: a double fork plus `setsid` leaves the command's process group
/// and outlives it. The session sweep must find it by its profile.
#[tokio::test]
async fn a_process_that_escapes_its_group_is_reaped_with_the_session() {
    skip_if_unsupported!();
    let fx = fixture();
    let (code, out) = run(
        &fx.sandbox,
        "/usr/bin/python3 -c '
import os, time
if os.fork() == 0:
    os.setsid()
    if os.fork() == 0:
        open(os.environ[\"TMPDIR\"] + \"/escaped\", \"w\").write(str(os.getpid()))
        time.sleep(300)
    os._exit(0)
'; sleep 1; cat \"$TMPDIR/escaped\"",
    )
    .await;
    assert_eq!(code, 0, "{out}");
    let pid: u32 = out
        .trim()
        .lines()
        .last()
        .and_then(|l| l.parse().ok())
        .expect("the escapee's pid");
    // First prove it escaped, or reaping it would prove nothing.
    assert!(alive(pid), "the escapee ({pid}) should outlive its command");

    fx.sandbox.stop().await;
    std::thread::sleep(Duration::from_millis(300));
    assert!(!alive(pid), "the session sweep missed {pid}");
}

/// A background process is reachable from later commands (a configured port, since
/// loopback is the host's) and stops with its whole tree.
#[tokio::test]
async fn a_background_process_serves_later_commands_and_stops() {
    skip_if_unsupported!();
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut security = SecurityConfig::default();
    security.sandbox.loopback_ports = vec![port];
    let fx = fixture_with(security, None, Arc::new(cowboy_gateway::DenyAll));
    std::fs::write(fx.project.path().join("index.html"), "served").unwrap();
    fx.sandbox
        .start_process(
            "web",
            &format!("/usr/bin/python3 -m http.server {port} --bind 127.0.0.1"),
            None,
        )
        .await
        .unwrap();

    let mut served = String::new();
    for _ in 0..40 {
        let (code, out) = run(
            &fx.sandbox,
            &format!(
                "/usr/bin/python3 -c 'import urllib.request as u; \
                 print(u.build_opener(u.ProxyHandler({{}})).open(\"http://127.0.0.1:{port}/index.html\").read())'"
            ),
        )
        .await;
        if code == 0 {
            served = out;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(
        served.contains("served"),
        "the dev server never answered: {served}"
    );

    fx.sandbox.stop_process("web").await.unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "the server outlived stop_process"
    );
    fx.sandbox.stop().await;
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            !s.trim().is_empty() && !s.trim_start().starts_with('Z')
        })
}
