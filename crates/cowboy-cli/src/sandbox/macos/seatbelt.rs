//! Applying the Seatbelt profile, in the shim, immediately before `exec`.
//!
//! `sandbox_init` is deprecated in the SDK headers and is still what `sandbox-exec`
//! calls; there is no supported replacement for confining an arbitrary process. It
//! takes the profile as SBPL source, fails with a message on a malformed one, and
//! can never be undone by the process it confines — verified on the target host.

use std::ffi::{c_char, c_int, CStr, CString};

use anyhow::{bail, Context, Result};

use crate::sandbox::shim::ShimRequest;

extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// Confine this process with the request's profile. Fails closed: with no profile,
/// an empty one, or one the kernel rejects, the command never runs.
pub fn apply(req: &ShimRequest) -> Result<()> {
    let Some(profile) = req
        .seatbelt_profile
        .as_deref()
        .filter(|p| !p.trim().is_empty())
    else {
        bail!("the shim request carries no Seatbelt profile; refusing to run unconfined");
    };

    // A session of its own, so the command has no controlling terminal: without one,
    // `TIOCSTI` cannot push keystrokes into the user's shell (bwrap's `--new-session`
    // on Linux). It also makes the command a process-group leader, which is what the
    // host signals to stop the whole tree.
    //
    // SAFETY: no arguments; fails only when already a group leader, which the host
    // never makes us.
    if unsafe { libc::setsid() } < 0 {
        return Err(std::io::Error::last_os_error()).context("starting a new session");
    }

    // No core files: a crashing command must not write its memory — which may hold
    // injected secrets — anywhere, least of all into the workspace.
    let none = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a valid rlimit for a valid resource.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &none) } != 0 {
        return Err(std::io::Error::last_os_error()).context("disabling core dumps");
    }

    let profile = CString::new(profile).context("the Seatbelt profile contains a NUL byte")?;
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: a NUL-terminated profile, flags 0 (the profile is SBPL source), and an
    // out-pointer the call fills on failure and we free below.
    let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut err) };
    if rc != 0 {
        let msg = if err.is_null() {
            "unknown error".to_string()
        } else {
            // SAFETY: on failure `err` is a NUL-terminated string owned by libsandbox.
            let m = unsafe { CStr::from_ptr(err) }
                .to_string_lossy()
                .into_owned();
            // SAFETY: freed exactly once, with the matching deallocator.
            unsafe { sandbox_free_error(err) };
            m
        };
        bail!("applying the Seatbelt profile failed: {}", msg.trim());
    }
    Ok(())
}
