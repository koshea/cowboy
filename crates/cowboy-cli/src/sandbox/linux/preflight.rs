//! The Linux prerequisites: bubblewrap, user namespaces, Landlock, seccomp, the
//! interception tools, and a delegated cgroup for the (non-boundary) limits.

use std::path::PathBuf;

use crate::sandbox::preflight::Requirement;
use crate::sandbox::transport::{EgressTransport, NftTransport, TransportConfig};

/// Ordered so that the most fundamental failure is reported first:
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
    match crate::sandbox::bwrap::resolve_bwrap() {
        Err(e) => Requirement::missing("bubblewrap", e.to_string(), install_hint("bubblewrap")),
        Ok(path) => match crate::sandbox::bwrap::ensure_not_setuid(&path) {
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
    let Ok(bwrap) = crate::sandbox::bwrap::resolve_bwrap() else {
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
    let required = crate::sandbox::lockdown::REQUIRED_ABI as i32;
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
///
/// The remedy names the **upstream project** rather than one distribution's package
/// atom. `emerge sys-apps/util-linux` is useless advice on Debian, and cowboy is
/// documented as Linux-only, not Gentoo-only — so the package hint is now labelled by
/// distro family and the project name is always given.
fn check_tools() -> Vec<Requirement> {
    const TOOLS: &[(&str, &str, &str)] = &[
        ("unshare", "creates the session's namespaces", "util-linux"),
        (
            "ip",
            "configures the sandbox's black-hole device",
            "iproute2",
        ),
        (
            "nft",
            "installs the egress interception ruleset",
            "nftables",
        ),
        (
            "sysctl",
            "enables loopback delivery for intercepted traffic",
            "procps",
        ),
    ];
    TOOLS
        .iter()
        .map(|(bin, why, pkg)| match which(bin) {
            Some(p) => Requirement::ok(bin, p.display().to_string()),
            None => Requirement::missing(bin, format!("not on PATH — {why}"), install_hint(pkg)),
        })
        .collect()
}

/// "install <project>" plus this machine's own package command, when we can tell which
/// one it is. Detected from the package managers actually present, so the hint is
/// copy-pasteable instead of aspirational.
pub(crate) fn install_hint(project: &str) -> String {
    let managers: &[(&str, &str)] = &[
        ("apt-get", "sudo apt install"),
        ("dnf", "sudo dnf install"),
        ("pacman", "sudo pacman -S"),
        ("zypper", "sudo zypper install"),
        ("emerge", "sudo emerge"),
        ("apk", "sudo apk add"),
        ("nix-env", "nix-env -iA nixpkgs."),
    ];
    match managers.iter().find(|(bin, _)| which(bin).is_some()) {
        Some((_, cmd)) if cmd.ends_with('.') => format!("install {project} (`{cmd}{project}`)"),
        Some((_, cmd)) => format!("install {project} (`{cmd} {project}`)"),
        None => format!("install {project}"),
    }
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
    let script = crate::sandbox::transport::nft::ruleset(&cfg);
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
    if crate::sandbox::cgroup::available() {
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

    /// The ABI query must agree with what lockdown demands, or doctor would pass a
    /// host on which every command then fails.
    #[test]
    fn the_landlock_abi_query_agrees_with_what_is_required() {
        let abi = landlock_abi().expect("this kernel has landlock");
        assert!(
            abi >= crate::sandbox::lockdown::REQUIRED_ABI as i32,
            "kernel ABI {abi} is below the required {}",
            crate::sandbox::lockdown::REQUIRED_ABI as i32
        );
    }

    #[test]
    fn a_bogus_module_is_not_present() {
        assert!(!module_present("definitely_not_a_module_xyz"));
    }
}
