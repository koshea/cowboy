//! Finding a session's processes without a PID namespace.
//!
//! On Linux a session's processes are exactly those in its namespaces, and the kernel
//! reaps them. macOS has nothing like that: a command that double-forks and calls
//! `setsid` is reparented to launchd, in a session of its own, and survives the kill
//! of the command's process group. It is still confined — Seatbelt is inherited and
//! cannot be dropped — but it is no longer anyone's child.
//!
//! What identifies it is its **profile**, which it cannot change. The kernel's own
//! `sandbox_check` answers "is this process in this session's sandbox?" with three
//! questions, all of which must hold:
//!
//! 1. it is sandboxed at all;
//! 2. its profile may read this session's scratch `TMPDIR` — no other cowboy
//!    session's profile may;
//! 3. its profile may **not** read the scratch directory's parent, which ours only
//!    lets it stat.
//!
//! The third is not decoration. Without it the sweep killed Apple's own agents —
//! WallpaperAgent, NotificationCenter, `secd`, `trustd` — whose sandbox profiles are
//! broad enough to read anything under `~/.cache`, and so passed the first two. A
//! profile that allows exactly this session's `tmp` and refuses the directory beside
//! it is one only this session writes. An environment marker would not do either,
//! because a command can clear its own environment. See `sandbox-decisions.md`.

use std::ffi::{c_char, c_int, CString};
use std::path::Path;

extern "C" {
    /// Variadic in C, and must be called as such: a caller that gets the ABI wrong
    /// (Python's `ctypes` on arm64, for one) silently gets every answer wrong.
    fn sandbox_check(pid: libc::pid_t, operation: *const c_char, filter: c_int, ...) -> c_int;
    /// Makes a check silent. Without it every probe of a process that is *not* ours —
    /// every sandboxed app the user runs — writes a denial to the system log.
    static SANDBOX_CHECK_NO_REPORT: c_int;
}

const SANDBOX_FILTER_NONE: c_int = 0;
const SANDBOX_FILTER_PATH: c_int = 1;

/// The kernel's BSD process record for `pid`.
pub(crate) fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: a zeroed plain-old-data struct is a valid out-buffer.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as c_int;
    // SAFETY: the buffer and its size describe one `proc_bsdinfo`.
    let n =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (n == size).then_some(info)
}

/// What identifies one session's processes; see the module docs.
pub(crate) struct Canary {
    /// The session's scratch `TMPDIR`: readable by its profile.
    inside: CString,
    /// Its parent: stat-able, never readable, under that same profile.
    outside: CString,
}

impl Canary {
    /// For the session whose (canonical) scratch `TMPDIR` is `tmp`.
    pub(crate) fn new(tmp: &Path) -> Option<Self> {
        let outside = tmp.parent()?;
        Some(Self {
            inside: CString::new(tmp.as_os_str().as_encoded_bytes()).ok()?,
            outside: CString::new(outside.as_os_str().as_encoded_bytes()).ok()?,
        })
    }
}

/// Whether `pid` may read `path`, per its sandbox profile.
fn may_read(pid: libc::pid_t, path: &CString) -> bool {
    // SAFETY: a plain call into libsandbox; the variadic path argument is a
    // NUL-terminated string that outlives the call.
    unsafe {
        sandbox_check(
            pid,
            c"file-read-data".as_ptr(),
            SANDBOX_FILTER_PATH | SANDBOX_CHECK_NO_REPORT,
            path.as_ptr(),
        ) == 0
    }
}

/// Whether `pid` runs under the profile of the session `canary` describes.
pub(crate) fn in_session(pid: u32, canary: &Canary) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: as above, with no filter argument.
    let sandboxed = unsafe { sandbox_check(pid, std::ptr::null(), SANDBOX_FILTER_NONE) } == 1;
    sandboxed && may_read(pid, &canary.inside) && !may_read(pid, &canary.outside)
}

/// Apple's own binaries, which the sweep never signals.
///
/// Belt and braces over [`in_session`], after that check once matched system agents:
/// a command could in principle leave one of these running, and it would survive the
/// sweep — confined still, and far cheaper than killing a process the user's session
/// depends on.
fn is_system_binary(pid: u32) -> bool {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: a buffer of the documented maximum size.
    let n = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
        )
    };
    if n <= 0 {
        // Unreadable: do not risk it.
        return true;
    }
    let path = String::from_utf8_lossy(&buf[..n as usize]);
    ["/System/", "/usr/libexec/", "/usr/sbin/", "/Library/Apple/"]
        .iter()
        .any(|p| path.starts_with(p))
}

/// Every pid on the system.
fn all_pids() -> Vec<u32> {
    // SAFETY: a null buffer asks for the required count.
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return Vec::new();
    }
    // Headroom for processes started between the two calls.
    let mut buf = vec![0 as libc::pid_t; n as usize + 64];
    let bytes = (buf.len() * std::mem::size_of::<libc::pid_t>()) as c_int;
    // SAFETY: the buffer holds `bytes` bytes of pids.
    let got = unsafe { libc::proc_listallpids(buf.as_mut_ptr().cast(), bytes) };
    buf.truncate(got.max(0) as usize);
    buf.into_iter()
        .filter_map(|p| u32::try_from(p).ok())
        .filter(|&p| p > 0)
        .collect()
}

/// Kill every process confined by the session whose scratch `TMPDIR` is `tmp`.
/// Returns how many were signalled.
///
/// Only this user's processes are looked at, and never this one. The check cannot
/// match anything unsandboxed, anything under another session's profile, or a
/// system agent whose profile reads broadly.
pub(crate) fn sweep(tmp: &Path) -> usize {
    let Some(canary) = Canary::new(tmp) else {
        return 0;
    };
    // SAFETY: no arguments, cannot fail.
    let uid = unsafe { libc::getuid() };
    let me = std::process::id();
    let mut killed = 0;
    for pid in all_pids() {
        if pid == me || bsd_info(pid).is_none_or(|i| i.pbi_uid != uid) {
            continue;
        }
        if in_session(pid, &canary) && !is_system_binary(pid) {
            // SAFETY: a pid that is ours; ESRCH from a race is harmless.
            if unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) } == 0 {
                killed += 1;
            }
        }
    }
    killed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bsd_info_reads_this_processs_real_parent() {
        // SAFETY: getppid takes no arguments and cannot fail.
        let expected = unsafe { libc::getppid() } as u32;
        let info = bsd_info(std::process::id()).expect("our own record is readable");
        assert_eq!(info.pbi_ppid, expected);
        assert!(bsd_info(u32::MAX).is_none());
    }

    /// An unsandboxed process — this test — is never in a session, whatever the path.
    #[test]
    fn an_unsandboxed_process_is_never_in_a_session() {
        let canary = Canary::new(Path::new("/usr/bin")).unwrap();
        assert!(!in_session(std::process::id(), &canary));
    }

    /// The regression test for the sweep that killed system agents: with no session
    /// running, a fresh scratch directory under the real cache location must match
    /// **no process at all** on this machine. Placed where session scratch really
    /// lives, since that is what broad system profiles happened to be able to read.
    #[test]
    fn a_fresh_session_canary_matches_no_running_process() {
        let base = crate::project::private_dir().expect("the private dir");
        let tmp = base
            .join("scratch")
            .join(format!("canary-test-{}", std::process::id()))
            .join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let tmp = std::fs::canonicalize(&tmp).unwrap();
        let canary = Canary::new(&tmp).unwrap();
        let matched: Vec<u32> = all_pids()
            .into_iter()
            .filter(|&p| in_session(p, &canary))
            .collect();
        let _ = std::fs::remove_dir_all(tmp.parent().unwrap());
        assert!(
            matched.is_empty(),
            "processes outside any session matched the sweep: {matched:?}"
        );
    }

    #[test]
    fn the_pid_list_includes_this_process() {
        assert!(all_pids().contains(&std::process::id()));
    }
}
