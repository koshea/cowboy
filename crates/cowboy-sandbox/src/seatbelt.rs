//! Rendering a [`SandboxPlan`] as a macOS Seatbelt (SBPL) profile.
//!
//! Pure, like the plan itself: the profile is a string computed from the plan, so
//! what the macOS boundary *is* can be snapshotted and reviewed on any host. The shim
//! applies it with `sandbox_init` immediately before `exec`, where — like a Landlock
//! domain — it can never be widened again.
//!
//! The shape is `(deny default)` and then allow rules, with two classes of deny
//! rendered **after** them because SBPL gives the later matching rule precedence:
//! read-only exposures nested inside a writable one (the ranch store inside the
//! project), and the config masks, which are always last. Every rule here was
//! verified on the target host; see the macOS section of
//! `docs/src/security/sandbox-decisions.md`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::plan::{BindMode, SandboxPlan};

/// Where the sandbox may send traffic. Session-specific, so not part of the plan.
#[derive(Debug, Clone)]
pub struct Network<'a> {
    /// This session's host proxy: the one outbound destination.
    pub proxy_port: u16,
    /// Loopback ports the command may connect to directly, bypassing the proxy
    /// (`network_policy.loopback_ports`).
    pub loopback_ports: &'a [u16],
}

/// Mach services the toolchain cannot run without, and nothing else.
///
/// Each was found by running the toolchain under `(deny default)` and reading the
/// denials: `getpwuid` (opendirectoryd), the per-user temp dir (dirhelper), and
/// logging. Deliberately absent: LaunchServices and coreservicesd (`open(1)`), the
/// pasteboard, `com.apple.dnssd.service` (DNS), and everything that would let a
/// command ask an *unsandboxed* process to act for it.
const MACH_SERVICES: &[&str] = &[
    "com.apple.system.opendirectoryd.libinfo",
    "com.apple.system.opendirectoryd.membership",
    "com.apple.system.notification_center",
    "com.apple.bsd.dirhelper",
    "com.apple.logd",
    "com.apple.diagnosticd",
];

/// Device nodes, read-write. Never the whole of `/dev`.
const DEVICES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
    "/dev/dtracehelper",
];

/// Top-level symlinks that path resolution has to be able to read.
const ROOT_SYMLINKS: &[&str] = &["/tmp", "/var", "/etc"];

/// Render the profile.
///
/// `canon` resolves a host path to the form Seatbelt matches against — the real
/// path, since `/tmp/x` is checked as `/private/tmp/x`. Tests pass the identity.
pub fn profile(plan: &SandboxPlan, net: &Network<'_>, canon: &dyn Fn(&Path) -> PathBuf) -> String {
    let mut ro: Vec<PathBuf> = Vec::new();
    let mut rw: Vec<PathBuf> = Vec::new();
    for b in &plan.binds {
        let p = canon(Path::new(&b.target));
        match b.mode {
            BindMode::ReadOnly => ro.push(p),
            BindMode::ReadWrite => rw.push(p),
        }
    }
    let masks: Vec<PathBuf> = plan.masks.iter().map(|m| canon(m)).collect();
    let links: Vec<PathBuf> = plan.home_links.iter().map(|(l, _)| canon(l)).collect();
    let xcrun = plan.xcrun_cache_prefix.as_deref().map(canon);

    let mut s = String::new();
    s.push_str("(version 1)\n(deny default)\n\n");

    s.push_str(";; processes: anything this sandbox runs, and only its own\n");
    s.push_str("(allow process-exec process-fork)\n");
    s.push_str("(allow signal (target same-sandbox))\n");
    s.push_str("(allow process-info* (target same-sandbox))\n");
    s.push_str("(allow sysctl-read)\n");
    s.push_str("(allow pseudo-tty)\n\n");

    s.push_str(";; ipc: what the toolchain needs to look up users and log\n");
    s.push_str("(allow mach-lookup");
    for m in MACH_SERVICES {
        s.push_str(&format!("\n  (global-name {})", quote(m)));
    }
    s.push_str(")\n");
    s.push_str(
        "(allow ipc-posix-shm-read-data (ipc-posix-name \"apple.shm.notification_center\"))\n\n",
    );

    s.push_str(";; the root and every ancestor of an exposed path: listable (the root)\n");
    s.push_str(";; and stat-able, but no more — anything else does not appear to exist\n");
    s.push_str("(allow file-read-data (literal \"/\"))\n");
    let mut ancestors: BTreeSet<PathBuf> = ROOT_SYMLINKS.iter().map(PathBuf::from).collect();
    for p in ro
        .iter()
        .chain(&rw)
        .chain(&masks)
        .chain(&links)
        .chain(xcrun.iter())
    {
        let mut cur = p.parent();
        while let Some(a) = cur {
            ancestors.insert(a.to_path_buf());
            cur = a.parent();
        }
    }
    // `/opt` is walked by `realpath` on the way to Homebrew.
    ancestors.insert(PathBuf::from("/opt"));
    s.push_str("(allow file-read-metadata");
    for a in &ancestors {
        s.push_str(&format!("\n  (literal {})", quote_path(a)));
    }
    s.push_str(")\n\n");

    s.push_str(";; devices\n(allow file-read* file-write* file-ioctl");
    for d in DEVICES {
        s.push_str(&format!("\n  (literal {})", quote(d)));
    }
    s.push_str("\n  (subpath \"/dev/fd\"))\n\n");

    if !ro.is_empty() {
        s.push_str(";; read-only\n(allow file-read*");
        for p in &ro {
            s.push_str(&format!("\n  (subpath {})", quote_path(p)));
        }
        s.push_str(")\n\n");
    }
    if !rw.is_empty() {
        s.push_str(";; read-write\n(allow file-read* file-write*");
        for p in &rw {
            s.push_str(&format!("\n  (subpath {})", quote_path(p)));
        }
        s.push_str(")\n\n");
    }
    if !links.is_empty() {
        // The links themselves live in the writable HOME; listed so that reading one
        // does not depend on that.
        s.push_str(";; credential links in the agent's HOME\n(allow file-read*");
        for l in &links {
            s.push_str(&format!("\n  (literal {})", quote_path(l)));
        }
        s.push_str(")\n\n");
    }
    if let Some(prefix) = &xcrun {
        // Read-only. The host's own `xcrun` trusts this cache to say where `git` and
        // `clang` are, so a sandbox that could write it could choose what the user's
        // next `git` runs, outside the sandbox. Reading it lets the shims skip a slow
        // lookup; a missing entry costs the sandbox one warning, not a failure.
        s.push_str(";; xcrun's lookup cache: read-only, since the host trusts it\n");
        s.push_str(&format!(
            "(allow file-read* (prefix {}))\n\n",
            quote_path(prefix)
        ));
    }

    // A read-only exposure inside a writable one — the ranch store inside the
    // project — must still refuse writes, and only a later deny can say so.
    let nested: Vec<&PathBuf> = ro
        .iter()
        .filter(|p| rw.iter().any(|w| p.starts_with(w) && *p != w))
        .collect();
    if !nested.is_empty() {
        s.push_str(";; read-only inside a writable path\n(deny file-write*");
        for p in nested {
            s.push_str(&format!("\n  (subpath {})", quote_path(p)));
        }
        s.push_str(")\n\n");
    }

    s.push_str(";; network: this session's proxy and nothing else outbound; no DNS\n");
    s.push_str("(allow network-bind network-inbound (local ip \"localhost:*\"))\n");
    s.push_str("(allow network-outbound");
    s.push_str(&format!("\n  (remote ip \"localhost:{}\")", net.proxy_port));
    for port in net.loopback_ports {
        s.push_str(&format!("\n  (remote ip \"localhost:{port}\")"));
    }
    s.push_str(")\n");
    if !rw.is_empty() {
        // The agent's own unix sockets (a dev server, a test harness) — never the
        // host's: ssh-agent, Docker and cowboyd all live outside these paths.
        s.push_str("(allow network-outbound network-bind");
        for p in &rw {
            s.push_str(&format!(
                "\n  (remote unix-socket (subpath {}))",
                quote_path(p)
            ));
        }
        s.push_str(")\n");
    }
    s.push('\n');

    if !masks.is_empty() {
        s.push_str(";; host-owned config: masked, and last so nothing above re-exposes it\n");
        s.push_str("(deny file-read* file-write*");
        for m in &masks {
            s.push_str(&format!("\n  (literal {})", quote_path(m)));
        }
        s.push_str(")\n");
    }
    s
}

/// An SBPL string literal.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn quote_path(p: &Path) -> String {
    quote(&p.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Grant, PlanInputs, Platform};
    use crate::probe::FakeHost;
    use cowboy_core::config::SecurityConfig;

    fn host() -> FakeHost {
        FakeHost {
            developer_bundle: Some(PathBuf::from("/Applications/Xcode.app")),
            darwin_user_temp: Some(PathBuf::from("/private/var/folders/xy/abc/T")),
            ..FakeHost::new().with_home("/Users/dev").with_existing([
                "/usr",
                "/bin",
                "/System",
                "/opt/homebrew",
                "/private/etc/hosts",
                "/Library/Preferences/com.apple.dt.Xcode.plist",
                "/Applications/Xcode.app",
                "/Users/dev/proj",
                "/Users/dev/proj/.cowboy/ranches",
                "/Users/dev/.cargo/bin",
                "/Users/dev/.rustup",
            ])
        }
    }

    fn plan_with(grants: &[Grant]) -> SandboxPlan {
        let sec = SecurityConfig::default();
        SandboxPlan::build(
            &PlanInputs {
                root: Path::new("/Users/dev/proj"),
                security: &sec,
                grants,
                mask_file: Path::new("/scratch/mask"),
                relay_port: 8443,
                scratch: Path::new("/scratch"),
                agent_home: Path::new("/Users/dev/.cache/cowboy/home/proj"),
                git_identity: None,
                platform: Platform::MacOs,
            },
            &host(),
        )
        .unwrap()
    }

    fn render(plan: &SandboxPlan) -> String {
        profile(
            plan,
            &Network {
                proxy_port: 51000,
                loopback_ports: &[5432],
            },
            &|p| p.to_path_buf(),
        )
    }

    #[test]
    fn snapshot_macos_profile() {
        insta::assert_snapshot!(render(&plan_with(&[])));
    }

    /// The whole point of the ordering: a deny only wins if it comes after the allow
    /// it overrides, so the masks must be the last rules in the profile.
    #[test]
    fn the_config_masks_are_the_last_rules() {
        let p = render(&plan_with(&[]));
        let mask = p
            .rfind("(deny file-read* file-write*")
            .expect("the masks are rendered");
        let last_allow = p.rfind("(allow ").unwrap();
        assert!(
            mask > last_allow,
            "a later allow would re-expose the mask:\n{p}"
        );
        assert!(
            p.contains("\"/Users/dev/proj/.cowboy/security.yaml\""),
            "{p}"
        );
        assert!(p.contains("\"/Users/dev/proj/.cowboy/models.yaml\""), "{p}");
    }

    /// Only the proxy, and the configured loopback ports, are reachable: no rule may
    /// allow outbound to anything that is not loopback.
    #[test]
    fn outbound_is_only_the_proxy_and_configured_loopback_ports() {
        let p = render(&plan_with(&[]));
        let remote_ips: Vec<&str> = p
            .lines()
            .filter(|l| l.contains("(remote ip"))
            .map(|l| l.trim().trim_end_matches(')'))
            .collect();
        assert_eq!(
            remote_ips,
            vec![
                "(remote ip \"localhost:51000\"",
                "(remote ip \"localhost:5432\""
            ],
            "{p}"
        );
        assert!(!p.contains("remote ip \"*"), "no wildcard destination: {p}");
        assert!(
            !p.contains("com.apple.dnssd"),
            "DNS is resolved by the host proxy, never in the sandbox: {p}"
        );
    }

    /// A read-only exposure nested in the writable project (the ranch store) stays
    /// read-only: without a later deny, the project's write rule would cover it.
    #[test]
    fn a_read_only_path_inside_the_project_refuses_writes() {
        let p = render(&plan_with(&[]));
        let deny = p
            .find("(deny file-write*\n  (subpath \"/Users/dev/proj/.cowboy/ranches\")")
            .expect("ranches must be write-denied");
        let rw = p.find(";; read-write").unwrap();
        assert!(deny > rw, "{p}");
    }

    /// The host's `xcrun` resolves `git` and `clang` through this cache, so a sandbox
    /// that could write it could pick what the user runs next, outside the sandbox.
    #[test]
    fn the_xcrun_cache_is_readable_but_never_writable() {
        let p = render(&plan_with(&[]));
        let line = p
            .lines()
            .find(|l| l.contains("xcrun_db"))
            .expect("the cache is readable, so the shims stay fast");
        assert!(line.starts_with("(allow file-read* (prefix"), "{line}");
        assert!(!line.contains("file-write"), "{line}");
    }

    /// Nothing outside the plan is granted by accident: every file rule names a path
    /// the plan exposes, a device, or an ancestor (metadata only).
    #[test]
    fn home_is_not_readable_only_its_exposed_parts() {
        let p = render(&plan_with(&[]));
        assert!(!p.contains("(subpath \"/Users/dev\")"), "{p}");
        assert!(
            p.contains("(literal \"/Users/dev\")"),
            "the home dir is an ancestor, so stat-able: {p}"
        );
        assert!(p.contains("(subpath \"/Users/dev/.rustup\")"), "{p}");
    }

    #[test]
    fn a_runtime_grant_is_in_the_next_profile() {
        let grant = Grant {
            path: PathBuf::from("/Users/dev/other"),
            read_only: false,
        };
        let p = render(&plan_with(std::slice::from_ref(&grant)));
        let rw = &p[p.find(";; read-write").unwrap()..];
        assert!(rw.contains("(subpath \"/Users/dev/other\")"), "{p}");
    }

    #[test]
    fn quoting_escapes_quotes_and_backslashes() {
        assert_eq!(quote(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
