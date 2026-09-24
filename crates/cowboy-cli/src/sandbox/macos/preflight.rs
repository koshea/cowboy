//! The macOS prerequisites, each checked by doing it: the OS release, that a Seatbelt
//! profile confines a process, and that a profile's network rule admits exactly the
//! port it names.
//!
//! The confinement checks run through `/usr/bin/sandbox-exec` — the same
//! `sandbox_init` the shim calls — rather than through the shim itself. That keeps
//! them independent of which binary is running: inside a unit test, "this binary"
//! is the test harness, which happily exits 0 when handed an argument it reads as a
//! test filter, and a check trusting that exit status passed while confining
//! nothing. The shim's own path is covered end to end by `tests/sandbox_macos.rs`.
//!
//! For the same reason no result here trusts an exit status alone: each command
//! prints a marker, and only the marker counts as evidence that the confined command
//! ran and saw what it reports.

use std::net::TcpListener;
use std::process::Command;

use crate::sandbox::preflight::Requirement;

/// The oldest macOS the sandbox is built and verified against.
pub const MIN_MAJOR: u32 = 26;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

pub fn check_all() -> Vec<Requirement> {
    vec![
        check_release(),
        check_confinement(),
        check_network_rule(),
        Requirement::warn(
            "resource limits",
            "macOS has no per-session equivalent of a cgroup — memory/CPU/process ceilings \
             will not apply (build parallelism still does)",
            "nothing to change; limits are not part of the boundary",
        ),
    ]
}

fn check_release() -> Requirement {
    const NAME: &str = "macOS release";
    let version = os_version();
    let major = version
        .as_deref()
        .and_then(|v| v.split('.').next())
        .and_then(|m| m.parse::<u32>().ok());
    let arch_ok = cfg!(target_arch = "aarch64");
    match (major, arch_ok) {
        (Some(m), true) if m >= MIN_MAJOR => Requirement::ok(
            NAME,
            format!("macOS {} on Apple silicon", version.unwrap_or_default()),
        ),
        (_, false) => Requirement::missing(
            NAME,
            "this build is for Apple silicon only",
            "run cowboy on an Apple silicon Mac",
        ),
        (m, _) => Requirement::missing(
            NAME,
            format!(
                "macOS {} is older than {MIN_MAJOR}, the oldest release the sandbox is \
                 verified on",
                m.map_or("(unknown)".to_string(), |m| m.to_string())
            ),
            format!("update to macOS {MIN_MAJOR} or later"),
        ),
    }
}

fn os_version() -> Option<String> {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: a byte buffer and its length for a string sysctl.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            buf.as_mut_ptr().cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    let s = std::str::from_utf8(&buf[..len]).ok()?;
    Some(s.trim_end_matches('\0').to_string())
}

/// Run `script` under `profile`, returning its stdout. An error means the profile
/// could not be applied at all — a broken host, not a confined command.
fn run_confined(profile: &str, script: &str) -> Result<String, String> {
    let out = Command::new(SANDBOX_EXEC)
        .args(["-p", profile, "/bin/sh", "-c", script])
        .output()
        .map_err(|e| format!("running {SANDBOX_EXEC}: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("sandbox-exec:") {
        return Err(stderr.trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A file the profile denies must be unreadable, and one it does not deny readable
/// — both, or the check proves nothing.
fn check_confinement() -> Requirement {
    const NAME: &str = "seatbelt";
    let dir = match tempdir() {
        Ok(d) => d,
        Err(e) => return Requirement::missing(NAME, e, "check that $TMPDIR is writable"),
    };
    let secret = dir.join("secret");
    let open = dir.join("open");
    for f in [&secret, &open] {
        if let Err(e) = std::fs::write(f, "x") {
            return Requirement::missing(NAME, e.to_string(), "check that $TMPDIR is writable");
        }
    }
    let secret = std::fs::canonicalize(&secret).unwrap_or(secret);
    let open = std::fs::canonicalize(&open).unwrap_or(open);
    let profile = format!(
        "(version 1)(allow default)(deny file-read* (literal \"{}\"))",
        secret.display()
    );
    let result = run_confined(
        &profile,
        &format!(
            "cat '{}' >/dev/null 2>&1 && echo SECRET-READ || echo SECRET-DENIED; \
             cat '{}' >/dev/null 2>&1 && echo OPEN-READ || echo OPEN-DENIED",
            secret.display(),
            open.display()
        ),
    );
    let _ = std::fs::remove_dir_all(&dir);
    match result {
        Err(e) => Requirement::missing(
            NAME,
            format!("a Seatbelt profile could not be applied: {e}"),
            "report this; the sandbox cannot run here",
        ),
        Ok(out) if out.contains("SECRET-DENIED") && out.contains("OPEN-READ") => {
            Requirement::ok(NAME, "a profile confines the command it is applied to")
        }
        Ok(out) if out.contains("SECRET-READ") => Requirement::missing(
            NAME,
            "a path the profile denies was readable — Seatbelt is not enforcing",
            "check that System Integrity Protection is enabled (`csrutil status`)",
        ),
        Ok(out) => Requirement::missing(
            NAME,
            format!(
                "the confined command did not run as expected: {:?}",
                out.trim()
            ),
            "check that /bin/sh and /bin/cat are intact",
        ),
    }
}

/// The profile's outbound rule must admit the port it names and refuse another.
///
/// Two loopback listeners of our own, so the check is meaningful offline: a refusal
/// is only evidence when a connection the same command made a moment earlier, under
/// the same profile, succeeded.
fn check_network_rule() -> Requirement {
    const NAME: &str = "egress confinement";
    let (Ok(allowed), Ok(other)) = (
        TcpListener::bind("127.0.0.1:0"),
        TcpListener::bind("127.0.0.1:0"),
    ) else {
        return Requirement::missing(NAME, "cannot bind loopback listeners", "check loopback");
    };
    let (Ok(a), Ok(o)) = (allowed.local_addr(), other.local_addr()) else {
        return Requirement::missing(NAME, "cannot read listener ports", "check loopback");
    };
    let profile = format!(
        "(version 1)(allow default)(deny network-outbound)\
         (allow network-outbound (remote ip \"localhost:{}\"))",
        a.port()
    );
    let result = run_confined(
        &profile,
        &format!(
            "/usr/bin/nc -z -G 3 127.0.0.1 {} && echo ALLOWED-REACHED || echo ALLOWED-REFUSED; \
             /usr/bin/nc -z -G 3 127.0.0.1 {} && echo OTHER-REACHED || echo OTHER-REFUSED",
            a.port(),
            o.port()
        ),
    );
    match result {
        Err(e) => Requirement::missing(
            NAME,
            format!("a Seatbelt profile could not be applied: {e}"),
            "report this; the sandbox cannot run here",
        ),
        Ok(out) if out.contains("ALLOWED-REACHED") && out.contains("OTHER-REFUSED") => {
            Requirement::ok(NAME, "only the proxy port is reachable")
        }
        Ok(out) if out.contains("OTHER-REACHED") => Requirement::missing(
            NAME,
            "a port the profile does not allow was reachable",
            "check that System Integrity Protection is enabled (`csrutil status`)",
        ),
        Ok(out) if out.contains("ALLOWED-REFUSED") => Requirement::missing(
            NAME,
            "the allowed port was not reachable, so the proxy would be unusable",
            "check for a firewall blocking loopback",
        ),
        Ok(out) => Requirement::missing(
            NAME,
            format!(
                "the confined command did not run as expected: {:?}",
                out.trim()
            ),
            "check that /usr/bin/nc is intact",
        ),
    }
}

/// A directory of this call's own. Not keyed on the pid alone: `cargo test` runs a
/// binary's tests as threads of one process, and two checks sharing a directory
/// deleted it out from under each other.
fn tempdir() -> Result<std::path::PathBuf, String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("cowboy-doctor-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}
