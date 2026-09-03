//! Host prerequisite checks for the sandbox, behind `cowboy doctor`.
//!
//! The container had one prerequisite — a working Docker — and the daemon reported
//! its own health. A host-native sandbox instead depends on several kernel features
//! that a distribution can each independently omit, so "why did this fail" needs an
//! answer that is more specific than "it did not start".
//!
//! Two principles here:
//!
//! - **Check by doing.** Wherever it is cheap, the check performs the real
//!   operation — spawn a user namespace, ask the kernel its Landlock ABI, load the
//!   real ruleset in a throwaway namespace. A check that reads a config symbol and
//!   infers the rest is the kind that passes on a machine where the feature does not
//!   work.
//! - **Say what to do.** A missing feature reports the remedy (a kernel option to
//!   set, a package to install), because the person reading this is being asked to
//!   change their machine.
//!
//! Nothing here is part of the security boundary. Every one of these features is
//! also checked at the point of use, where failing closed is what matters; this is
//! for diagnosis.

use std::path::PathBuf;

use super::transport::{EgressTransport, NftTransport, TransportConfig};

/// How a prerequisite came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Present and working.
    Ok,
    /// Works, but degraded — a capability is unavailable without blocking use.
    Warn,
    /// The sandbox cannot run without this.
    Missing,
}

/// One prerequisite and what we found.
#[derive(Debug, Clone)]
pub struct Requirement {
    pub name: &'static str,
    pub state: State,
    /// What was found, phrased for someone who did not write this code.
    pub detail: String,
    /// What to do about it, when there is something to do.
    pub remedy: Option<String>,
}

impl Requirement {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            state: State::Ok,
            detail: detail.into(),
            remedy: None,
        }
    }
    fn warn(name: &'static str, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            name,
            state: State::Warn,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }
    fn missing(name: &'static str, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            name,
            state: State::Missing,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }
}

/// Run every check. Ordered so that the most fundamental failure is reported first:
/// if user namespaces are unavailable nothing else matters, and a reader should not
/// have to work out which of six failures is the cause of the others.
pub fn check_all() -> Vec<Requirement> {
    let mut out = vec![check_bwrap(), check_user_namespaces()];
    // Only worth asking about the rest once a sandbox can exist at all.
    out.push(check_landlock());
    out.push(check_seccomp());
    out.extend(check_tools());
    out.push(check_interception());
    out.push(check_limits());
    out
}

/// bubblewrap, and that it is *not* setuid.
///
/// A setuid bwrap is refused at the point of use, so this reports the same judgement
/// early. It matters because a setuid helper reintroduces a privileged component the
/// design deliberately does not have: everything here works as an ordinary user.
fn check_bwrap() -> Requirement {
    match super::bwrap::resolve_bwrap() {
        Err(e) => Requirement::missing(
            "bubblewrap",
            e.to_string(),
            "install bubblewrap (Gentoo: `emerge sys-apps/bubblewrap`)",
        ),
        Ok(path) => match super::bwrap::ensure_not_setuid(&path) {
            Err(e) => Requirement::missing(
                "bubblewrap",
                e.to_string(),
                "install a non-setuid build; cowboy needs no privileged helper",
            ),
            Ok(()) => Requirement::ok("bubblewrap", format!("{} (not setuid)", path.display())),
        },
    }
}

/// Unprivileged user namespaces, checked by creating one.
fn check_user_namespaces() -> Requirement {
    const NAME: &str = "user namespaces";
    let Ok(bwrap) = super::bwrap::resolve_bwrap() else {
        return Requirement::missing(
            NAME,
            "cannot check without bubblewrap",
            "install bubblewrap first",
        );
    };
    let ok = std::process::Command::new(&bwrap)
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
        .unwrap_or(false);
    if ok {
        return Requirement::ok(NAME, "unprivileged namespaces work");
    }
    // The usual cause is a distribution or hardening patch disabling them.
    let sysctl = std::fs::read_to_string("/proc/sys/user/max_user_namespaces")
        .ok()
        .map(|s| s.trim().to_string());
    let detail = match sysctl.as_deref() {
        Some("0") => "disabled (user.max_user_namespaces = 0)".to_string(),
        _ => "creating a user namespace failed".to_string(),
    };
    Requirement::missing(
        NAME,
        detail,
        "enable CONFIG_USER_NS and set `sysctl user.max_user_namespaces=<n>` (n > 0)",
    )
}

/// Landlock, and whether the ABI is new enough.
///
/// The required ABI is a hard requirement at the point of use rather than a
/// best-effort degradation, precisely so confinement cannot quietly enforce less
/// than it claims — which makes this the check that says so in advance.
fn check_landlock() -> Requirement {
    const NAME: &str = "landlock";
    let required = super::lockdown::REQUIRED_ABI as i32;
    match landlock_abi() {
        None => Requirement::missing(
            NAME,
            "not available in this kernel",
            "enable CONFIG_SECURITY_LANDLOCK and add `landlock` to CONFIG_LSM",
        ),
        Some(v) if v < required => Requirement::missing(
            NAME,
            format!("ABI {v}, but {required} is required"),
            format!("a kernel providing Landlock ABI {required} or newer (Linux 6.10+)"),
        ),
        Some(v) => Requirement::ok(NAME, format!("ABI {v} (>= {required})")),
    }
}

/// Ask the kernel its Landlock ABI version.
///
/// Done with the raw syscall because the `landlock` crate keeps its equivalent
/// private (deliberately — exposing it invites building rules against an ABI the
/// crate does not know). `None` means Landlock is absent or disabled.
fn landlock_abi() -> Option<i32> {
    /// `LANDLOCK_CREATE_RULESET_VERSION`: ask for the version instead of a ruleset.
    const VERSION_FLAG: u32 = 1;
    // SAFETY: a query call — a null attr pointer with size 0 and the version flag is
    // the documented way to ask, and it creates nothing.
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            VERSION_FLAG,
        )
    };
    (v > 0).then_some(v as i32)
}

/// seccomp filtering, which carries the io_uring and raw-socket denials.
fn check_seccomp() -> Requirement {
    const NAME: &str = "seccomp";
    // `actions_avail` exists only with CONFIG_SECCOMP_FILTER, and naming the actions
    // is more useful than a yes/no.
    match std::fs::read_to_string("/proc/sys/kernel/seccomp/actions_avail") {
        Ok(actions) => {
            let actions = actions.trim();
            if actions.contains("errno") {
                Requirement::ok(NAME, format!("filtering available ({actions})"))
            } else {
                Requirement::missing(
                    NAME,
                    format!("no errno action ({actions})"),
                    "a kernel with the standard seccomp actions",
                )
            }
        }
        Err(_) => Requirement::missing(
            NAME,
            "filtering unavailable",
            "enable CONFIG_SECCOMP and CONFIG_SECCOMP_FILTER",
        ),
    }
}

/// Command-line tools the sandbox shells out to.
fn check_tools() -> Vec<Requirement> {
    const TOOLS: &[(&str, &str, &str)] = &[
        (
            "unshare",
            "creates the session's namespaces",
            "sys-apps/util-linux",
        ),
        (
            "ip",
            "configures the sandbox's black-hole device",
            "sys-apps/iproute2",
        ),
        (
            "nft",
            "installs the egress interception ruleset",
            "net-firewall/nftables",
        ),
        (
            "sysctl",
            "enables loopback delivery for intercepted traffic",
            "sys-apps/procps",
        ),
    ];
    TOOLS
        .iter()
        .map(|(bin, why, pkg)| match which(bin) {
            Some(p) => Requirement::ok(bin, p.display().to_string()),
            None => Requirement::missing(
                bin,
                format!("not on PATH — {why}"),
                format!("install {pkg}"),
            ),
        })
        .collect()
}

/// Whether egress interception can actually be installed, checked by installing it
/// in a throwaway namespace.
///
/// This is the check worth having. The kernel modules it needs all autoload, so
/// inspecting `/proc/modules` says almost nothing; loading the real ruleset in a
/// namespace that is discarded immediately afterwards says everything, and costs one
/// process.
fn check_interception() -> Requirement {
    const NAME: &str = "egress interception";
    let cfg = TransportConfig::default();
    let script = super::transport::nft::ruleset(&cfg);
    let out = std::process::Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "--", "nft", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(script.as_bytes());
            }
            child.wait_with_output()
        });
    match out {
        Ok(o) if o.status.success() => Requirement::ok(
            NAME,
            format!("{} ruleset loads", NftTransport::new(cfg).name()),
        ),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            let missing: Vec<&str> = NftTransport::new(cfg)
                .requirements()
                .into_iter()
                .filter(|r| !module_present(r.module))
                .map(|r| r.config)
                .collect();
            let remedy = if missing.is_empty() {
                "check `nft` and unprivileged namespace support".to_string()
            } else {
                format!("enable {} in the kernel", missing.join(", "))
            };
            Requirement::missing(NAME, format!("ruleset failed to load: {err}"), remedy)
        }
        Err(e) => Requirement::missing(
            NAME,
            format!("could not run the check: {e}"),
            "install util-linux and nftables",
        ),
    }
}

/// Whether a module is loaded or built in. Only consulted to explain a failure —
/// most of these autoload on demand, so absence here is not itself a problem.
fn module_present(name: &str) -> bool {
    PathBuf::from("/sys/module")
        .join(name.replace('-', "_"))
        .exists()
}

/// Resource limits, which need a delegated cgroup v2 subtree.
///
/// A warning rather than a failure: limits protect the machine from a runaway build,
/// but the sandbox confines correctly without them, and the boundary does not depend
/// on them.
fn check_limits() -> Requirement {
    const NAME: &str = "resource limits";
    if super::cgroup::available() {
        Requirement::ok(NAME, "cgroup v2 subtree delegated (memory/cpu/pids)")
    } else {
        Requirement::warn(
            NAME,
            "no delegated cgroup v2 subtree — memory/CPU/process ceilings will not apply",
            "run under a systemd user session, or delegate `cpu memory pids` in the \
             cgroup subtree that owns this process",
        )
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The suite runs on the target host, so these must all pass here. If one does
    /// not, the sandbox tests would be skipping and the failure belongs in `doctor`
    /// rather than being discovered later.
    #[test]
    fn the_host_meets_every_requirement() {
        let checks = check_all();
        assert!(!checks.is_empty());
        let broken: Vec<_> = checks
            .iter()
            .filter(|c| c.state == State::Missing)
            .map(|c| {
                format!(
                    "{}: {} — {}",
                    c.name,
                    c.detail,
                    c.remedy.as_deref().unwrap_or("")
                )
            })
            .collect();
        assert!(
            broken.is_empty(),
            "this host cannot run the sandbox: {broken:#?}"
        );
    }

    /// Every non-Ok result must say what to do. A check that reports a problem and
    /// leaves the reader to guess is not much better than the failure it replaced.
    #[test]
    fn anything_not_ok_carries_a_remedy() {
        for c in check_all() {
            match c.state {
                State::Ok => assert!(
                    c.remedy.is_none(),
                    "{}: an ok check needs no remedy",
                    c.name
                ),
                _ => assert!(
                    c.remedy.as_deref().is_some_and(|r| !r.trim().is_empty()),
                    "{} reported a problem with no remedy",
                    c.name
                ),
            }
        }
    }

    /// The ABI query must agree with what lockdown demands, or doctor would pass a
    /// host on which every command then fails.
    #[test]
    fn the_landlock_abi_query_agrees_with_what_is_required() {
        let abi = landlock_abi().expect("this kernel has landlock");
        assert!(
            abi >= super::super::lockdown::REQUIRED_ABI as i32,
            "kernel ABI {abi} is below the required {}",
            super::super::lockdown::REQUIRED_ABI as i32
        );
    }

    #[test]
    fn a_bogus_module_is_not_present() {
        assert!(!module_present("definitely_not_a_module_xyz"));
    }
}
