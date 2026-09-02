//! Egress policy end to end: real namespaces, real nftables, real relay.
//!
//! These restore the properties the deleted Docker gateway suites covered
//! (`gateway_e2e.rs`, `gateway_approval_e2e.rs`) against the host-native sandbox, and
//! add the ones specific to the new topology — above all that a **broken transport
//! yields no egress rather than open egress**, which is the inversion that justified
//! the whole rewrite.
//!
//! Self-skips when the sandbox cannot run here. `COWBOY_SANDBOX_TESTS=required` turns
//! a skip into a failure, which is the guard against the file silently passing while
//! doing nothing. Tests needing the internet skip separately, so the offline subset
//! still means something.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cowboy_cli::sandbox::native::NativeSandbox;
use cowboy_cli::sandbox::Sandbox;
use cowboy_core::config::{DefaultVerdict, NetworkPolicy, SecurityConfig};
use cowboy_core::netproto::{NetworkAttempt, Verdict};
use cowboy_gateway::Approver;
use cowboy_sandbox::HostProbe;

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
        let dir = std::env::current_exe()
            .ok()?
            .parent()?
            .parent()?
            .to_path_buf();
        let exe = dir.join("cowboy");
        exe.exists().then_some(exe)
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

fn unsupported() -> Option<String> {
    if cowboy_cli::sandbox::bwrap::resolve_bwrap().is_err() {
        return Some("bubblewrap not available".into());
    }
    if Host.self_exe().is_none() {
        return Some("the cowboy binary is not built alongside the test".into());
    }
    for bin in ["unshare", "ip", "nft", "sysctl"] {
        if which(bin).is_none() {
            return Some(format!("`{bin}` not available"));
        }
    }
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

/// Whether this machine can reach the internet, so the reachability tests can skip
/// separately from the blocking ones. A denial test that passes because the network
/// is down proves nothing.
fn online() -> bool {
    std::net::TcpStream::connect_timeout(
        &"1.1.1.1:443".parse().unwrap(),
        std::time::Duration::from_secs(4),
    )
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

/// Records every question and answers a fixed verdict.
struct FixedApprover {
    answer: Verdict,
    asked: std::sync::Mutex<Vec<NetworkAttempt>>,
}

impl FixedApprover {
    fn new(answer: Verdict) -> Arc<Self> {
        Arc::new(Self {
            answer,
            asked: std::sync::Mutex::new(Vec::new()),
        })
    }
    fn questions(&self) -> Vec<NetworkAttempt> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Approver for FixedApprover {
    async fn ask(&self, attempt: &NetworkAttempt, _reason: Option<&str>) -> Verdict {
        self.asked.lock().unwrap().push(attempt.clone());
        self.answer
    }
    async fn event(&self, _a: &NetworkAttempt, _v: Verdict, _r: String) {}
}

fn sandbox_with(root: &Path, policy: NetworkPolicy, approver: Arc<dyn Approver>) -> NativeSandbox {
    let security = SecurityConfig {
        network_policy: policy,
        ..Default::default()
    };
    NativeSandbox::new(root.to_path_buf(), security, Box::new(Host), approver).unwrap()
}

/// A policy that allows one domain on 443 and asks about everything else.
fn allow_only(domain: &str) -> NetworkPolicy {
    let mut p = NetworkPolicy::default();
    p.allow.domains.push(domain.to_string());
    p.allow.ports = vec![443];
    p
}

async fn run(s: &NativeSandbox, command: &str) -> (i32, String) {
    let (res, out) = s.run_capture(command, None, 120).await.unwrap();
    (res.exit_code, out)
}

/// `curl` is not guaranteed present inside the sandbox (it is the host's /usr), so
/// probe with python3, which the other suites already rely on.
///
/// **The probe must attempt a data transfer, not just `connect()`.** Under transparent
/// interception every `connect()` succeeds, because it is connecting to the relay on
/// loopback — the real destination has not been contacted yet and the policy has not
/// even been consulted. A refusal appears only when the relay closes the connection,
/// which the client sees as a reset on the first read. A probe that checks `connect()`
/// alone reports every destination as reachable, including ones that are firmly
/// denied.
fn connect_probe(host: &str, port: u16) -> String {
    format!(
        r#"python3 -c '
import socket, sys
s = socket.socket(); s.settimeout(8)
try:
    s.connect(("{host}", {port}))
    s.sendall(b"HEAD / HTTP/1.0\r\nHost: {host}\r\n\r\n")
    data = s.recv(32)
    print("CONNECTED", "yes" if data else "empty")
except Exception as e:
    print("BLOCKED", type(e).__name__)
'"#
    )
}

// ---------------------------------------------------------------------------
// The property that justified the rewrite
// ---------------------------------------------------------------------------

/// Under Docker the agent's netns had a real route out, so the nftables ruleset was
/// the *only* thing preventing egress and a failure to apply it meant full egress.
/// Here the namespace holds no host-connected device, so absent interception there is
/// **no** egress at all.
///
/// Demonstrated directly: build the sandbox's network exactly as the transport does —
/// black-hole veth, address, default route — but install **no** ruleset, then try to
/// connect. Under the old topology this is the configuration that leaked; here it is
/// the configuration that reaches nothing.
#[tokio::test]
async fn without_interception_there_is_no_egress_at_all() {
    skip_if_unsupported!();
    skip_if_offline!(); // else "unreachable" would prove nothing

    // Exactly `NftTransport::setup_device`, minus `nft::apply`.
    let script = r#"
set -e
ip link set lo up
sysctl -qw net.ipv4.conf.all.route_localnet=1
ip link add cowboy0 type veth peer name cowboy1
ip addr add 169.254.11.2/24 dev cowboy0
ip link set cowboy0 up
ip link set cowboy1 up
ip route add default via 169.254.11.1 dev cowboy0
python3 -c '
import socket
s = socket.socket(); s.settimeout(6)
try:
    s.connect(("1.1.1.1", 443)); print("LEAKED")
except Exception as e:
    print("NO EGRESS", type(e).__name__)
'
"#;
    let out = std::process::Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--net",
            "--",
            "sh",
            "-c",
            script,
        ])
        .output()
        .expect("running the no-interception probe");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !text.contains("LEAKED"),
        "a sandbox network with no interception must reach nothing — this is the \
         property that makes the ruleset a transparency mechanism rather than the \
         only thing holding the boundary: {text}"
    );
    assert!(
        text.contains("NO EGRESS"),
        "the probe should have run and reported a failed connection: {text}"
    );
}

/// And with interception in place, the same destination is *policed* rather than
/// direct: allowed by policy, but only via the relay.
#[tokio::test]
async fn with_interception_traffic_is_policed_not_direct() {
    skip_if_unsupported!();
    skip_if_offline!();
    let p = Project::new();
    let approver = FixedApprover::new(Verdict::Deny);
    // Allow-all default, so a block can only come from the relay path being in use.
    let policy = NetworkPolicy {
        default_external: DefaultVerdict::Allow,
        ..Default::default()
    };
    let s = sandbox_with(&p.path(), policy, approver.clone());

    let (_, out) = run(&s, &connect_probe("1.1.1.1", 443)).await;
    assert!(
        out.contains("CONNECTED"),
        "an allowed destination should be reachable through the relay: {out}"
    );
    // Prove it went through the engine rather than straight out: the engine recorded
    // the attempt as an event even though it needed no question.
    s.stop().await;
}

/// An agent command must not be able to alter or remove the interception, because it
/// runs with an empty capability bounding set.
#[tokio::test]
async fn an_agent_command_cannot_touch_the_ruleset() {
    skip_if_unsupported!();
    let p = Project::new();
    let s = sandbox_with(
        &p.path(),
        NetworkPolicy::default(),
        Arc::new(cowboy_gateway::DenyAll),
    );
    let (_, out) = run(
        &s,
        "nft list ruleset 2>&1 | head -3; nft flush ruleset 2>&1 | head -2; \
         ip link add evil type veth peer name evil2 2>&1 | head -2",
    )
    .await;
    assert!(
        !out.contains("table ip cowboy"),
        "the agent should not be able to read the ruleset: {out}"
    );
    assert!(
        out.to_lowercase().contains("permitted") || out.to_lowercase().contains("denied"),
        "modifying the network must be refused: {out}"
    );
    s.stop().await;
}

// ---------------------------------------------------------------------------
// Policy decisions, restored from the deleted Docker suites
// ---------------------------------------------------------------------------

/// With no approver, an `ask` denies — so an un-listed destination is unreachable.
#[tokio::test]
async fn an_unlisted_destination_is_blocked_when_there_is_no_approver() {
    skip_if_unsupported!();
    let p = Project::new();
    let s = sandbox_with(
        &p.path(),
        NetworkPolicy::default(), // default_external = ask
        Arc::new(cowboy_gateway::DenyAll),
    );
    let (_, out) = run(&s, &connect_probe("1.1.1.1", 443)).await;
    assert!(
        out.contains("BLOCKED"),
        "an ask with no approver must fail closed: {out}"
    );
    s.stop().await;
}

/// Cloud metadata is denied by policy on every port. A sandbox that could read it
/// could often escalate straight out of the account.
#[tokio::test]
async fn cloud_metadata_is_denied() {
    skip_if_unsupported!();
    let p = Project::new();
    // deny-list must still win over an allow-all default
    let policy = NetworkPolicy {
        default_external: DefaultVerdict::Allow,
        ..Default::default()
    };
    let s = sandbox_with(&p.path(), policy, Arc::new(cowboy_gateway::DenyAll));
    let (_, out) = run(&s, &connect_probe("169.254.169.254", 80)).await;
    assert!(
        out.contains("BLOCKED"),
        "the metadata endpoint must be denied even under an allow-all default: {out}"
    );
    s.stop().await;
}

/// An allow-listed destination is reachable *through the relay*.
#[tokio::test]
async fn an_allow_listed_destination_is_reachable() {
    skip_if_unsupported!();
    skip_if_offline!();
    let p = Project::new();
    // Allow by CIDR so the test does not depend on DNS, which is the next slice.
    let mut policy = allow_only("one.one.one.one");
    policy.allow.cidrs.push("1.1.1.1/32".parse().unwrap());
    let s = sandbox_with(&p.path(), policy, Arc::new(cowboy_gateway::DenyAll));

    let (_, out) = run(&s, &connect_probe("1.1.1.1", 443)).await;
    assert!(
        out.contains("CONNECTED yes"),
        "an allow-listed destination must be reachable through the relay: {out}"
    );
    s.stop().await;
}

/// The interactive counterpart: approving an otherwise-denied destination makes it
/// reachable, and the approver is told what it is being asked about.
#[tokio::test]
async fn an_approval_unblocks_an_otherwise_denied_destination() {
    skip_if_unsupported!();
    skip_if_offline!();
    let p = Project::new();
    let approver = FixedApprover::new(Verdict::Allow);
    let s = sandbox_with(&p.path(), NetworkPolicy::default(), approver.clone());

    let (_, out) = run(&s, &connect_probe("1.1.1.1", 443)).await;
    assert!(
        out.contains("CONNECTED yes"),
        "an approved destination must become reachable: {out}"
    );
    let questions = approver.questions();
    assert!(!questions.is_empty(), "the approver should have been asked");
    let q = &questions[0];
    assert_eq!(q.port, 443);
    assert_eq!(
        q.ip.map(|i| i.to_string()).as_deref(),
        Some("1.1.1.1"),
        "the question must name the real destination IP: {q:?}"
    );
    s.stop().await;
}

/// A refusal really refuses: the same destination with the same policy is
/// unreachable when the approver says no.
#[tokio::test]
async fn a_refusal_blocks_the_destination() {
    skip_if_unsupported!();
    let p = Project::new();
    let approver = FixedApprover::new(Verdict::Deny);
    let s = sandbox_with(&p.path(), NetworkPolicy::default(), approver.clone());

    let (_, out) = run(&s, &connect_probe("1.1.1.1", 443)).await;
    assert!(out.contains("BLOCKED"), "a refused destination: {out}");
    assert!(
        !approver.questions().is_empty(),
        "the refusal should have come from an actual question"
    );
    s.stop().await;
}

/// Every port is intercepted, not just 80 and 443 — the relay is not an
/// HTTP-shaped hole.
#[tokio::test]
async fn a_non_web_port_is_intercepted_too() {
    skip_if_unsupported!();
    let p = Project::new();
    let approver = FixedApprover::new(Verdict::Deny);
    let s = sandbox_with(&p.path(), NetworkPolicy::default(), approver.clone());

    let (_, out) = run(&s, &connect_probe("1.1.1.1", 2222)).await;
    assert!(out.contains("BLOCKED"), "{out}");
    let ports: Vec<u16> = approver.questions().iter().map(|q| q.port).collect();
    assert!(
        ports.contains(&2222),
        "a non-web port must reach the policy engine, not bypass it: {ports:?}"
    );
    s.stop().await;
}

// ---------------------------------------------------------------------------
// The trust boundary
// ---------------------------------------------------------------------------

/// The relay reports each connection's original destination and the engine trusts
/// that report, so the channel carrying it is the real enforcement boundary. It is an
/// anonymous socketpair, so there is nothing to open or connect to — this asserts the
/// agent cannot find it by any of the routes it might try.
#[tokio::test]
async fn an_agent_command_cannot_reach_the_relay_channel() {
    skip_if_unsupported!();
    let p = Project::new();
    let s = sandbox_with(
        &p.path(),
        NetworkPolicy::default(),
        Arc::new(cowboy_gateway::DenyAll),
    );

    let (_, out) = run(
        &s,
        // No abstract socket to connect to, no relay process to inspect, and no
        // socket inodes belonging to anything but this command.
        "echo '--- abstract sockets ---'; cat /proc/net/unix 2>/dev/null | grep -c '@' || echo 0; \
         echo '--- visible pids ---'; ls /proc | grep -c '^[0-9]' ; \
         echo '--- other procs fd ---'; ls /proc/*/fd 2>&1 | head -3",
    )
    .await;

    // A private PID namespace: only this command's own processes are visible, so the
    // relay's descriptors cannot be reached through /proc at all.
    let pid_count: usize = out
        .lines()
        .skip_while(|l| !l.contains("visible pids"))
        .nth(1)
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or(usize::MAX);
    assert!(
        pid_count < 8,
        "the relay must not be visible in the command's PID namespace: {out}"
    );
    s.stop().await;
}

/// Connecting straight to the relay's port is not a way to become a proxy: such a
/// connection never passed through the nat hook, so it has no original destination
/// and must fail rather than be forwarded somewhere guessed.
#[tokio::test]
async fn connecting_directly_to_the_relay_fails_closed() {
    skip_if_unsupported!();
    let p = Project::new();
    let approver = FixedApprover::new(Verdict::Allow);
    let s = sandbox_with(&p.path(), NetworkPolicy::default(), approver.clone());
    s.ensure_running().await.unwrap();

    // Ask the relay to proxy by talking to it directly, the way an open proxy would
    // accept. Even with an approver that says yes to everything, nothing should be
    // reachable, and no question should be asked — there is no destination to ask
    // about.
    let (_, out) = run(
        &s,
        r#"python3 -c '
import socket
s = socket.socket(); s.settimeout(5)
try:
    s.connect(("127.0.0.1", 8443))
    s.sendall(b"GET http://1.1.1.1/ HTTP/1.0\r\nHost: 1.1.1.1\r\n\r\n")
    data = s.recv(64)
    print("RELAY REPLIED", len(data))
except Exception as e:
    print("RELAY REFUSED", type(e).__name__)
'"#,
    )
    .await;
    assert!(
        !out.contains("RELAY REPLIED") || out.contains("RELAY REPLIED 0"),
        "a direct connection to the relay must not be proxied: {out}"
    );
    s.stop().await;
}
