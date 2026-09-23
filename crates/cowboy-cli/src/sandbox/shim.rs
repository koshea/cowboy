//! `cowboy x-sandbox-shim` — the last thing that runs before the agent's command.
//!
//! bwrap sets up namespaces, binds and `pivot_root`, but it cannot apply Landlock
//! or a seccomp filter of our choosing. This shim runs *inside* the finished
//! sandbox and applies them immediately before `exec`, which is the only place they
//! can go: a Landlock domain can never be widened, so it must be installed after
//! all setup is complete and before any untrusted code runs.
//!
//! It receives its instructions on **stdin** as JSON rather than via argv, because
//! argv is visible in `/proc/<pid>/cmdline` to anything that can see the process,
//! and a long argv is awkward to get right through two layers of process spawning.
//!
//! The lockdown itself lands in the next slice; today the shim establishes the
//! plumbing and execs the command.

use std::os::unix::process::CommandExt;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// What the shim must do before exec. Sent on stdin as one JSON object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShimRequest {
    /// The command, run via `sh -c` so ordinary shell syntax works.
    pub command: String,
    /// Paths the Landlock domain may read.
    #[serde(default)]
    pub read_only: Vec<String>,
    /// Paths the Landlock domain may read and write.
    #[serde(default)]
    pub read_write: Vec<String>,
    /// Directories the Landlock domain may open and list, but not read files in.
    #[serde(default)]
    pub list_dirs: Vec<String>,
    /// Scope the domain against signalling and abstract sockets outside it.
    #[serde(default)]
    pub scope_ipc: bool,
    /// Syscalls the seccomp filter must refuse.
    #[serde(default)]
    pub deny_syscalls: Vec<String>,
    /// Refuse raw/datagram inet sockets.
    #[serde(default)]
    pub deny_raw_sockets: bool,
}

/// Read the request, apply lockdown, and exec the command.
///
/// Never returns on success — it becomes the command, so there is no shim process
/// left to inspect or signal, and the exit status is the command's own.
pub fn run() -> Result<()> {
    let req = read_request().context("reading the shim request from stdin")?;

    apply_lockdown(&req)?;

    // `sh -c` so the agent's command can use ordinary shell syntax. exec so the
    // shim leaves no trace and signals reach the command directly.
    let err = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&req.command)
        .exec();
    // `exec` only returns on failure.
    Err(anyhow::Error::new(err).context("exec of the sandboxed command failed"))
}

/// Read exactly one newline-terminated JSON object from **fd 0 directly**.
///
/// Byte-at-a-time, and crucially **not** through `std::io::stdin()`: `Stdin` wraps
/// an 8 KiB `BufReader`, so the first read slurps everything available on the pipe
/// — including the *payload* that follows the request line — into a userspace
/// buffer that is then thrown away when we `exec`. The exec'd command would inherit
/// fd 0 positioned at EOF and see empty stdin (observed as the structured file
/// tools failing with "parsing fileop request: EOF while parsing a value").
///
/// Reading the raw fd leaves the payload in the kernel pipe buffer, exactly where
/// the exec'd command's own stdin read will find it. A few hundred single-byte
/// `read(2)` syscalls is nothing next to the process spawn around it.
fn read_request() -> Result<ShimRequest> {
    let mut line = Vec::with_capacity(4096);
    let mut byte = [0u8; 1];
    loop {
        // SAFETY: reading one byte from fd 0 (stdin) into a stack buffer. fd 0 is
        // this process's stdin for the whole call. Going through libc rather than
        // `std::io::stdin()` is the whole point — see the doc comment.
        let n = unsafe { libc::read(0, byte.as_mut_ptr() as *mut libc::c_void, 1) };
        match n {
            0 => break, // EOF without a newline: parse what we have
            1 if byte[0] == b'\n' => break,
            1 => line.push(byte[0]),
            _ => {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e).context("reading the shim request from stdin");
            }
        }
    }
    serde_json::from_slice(&line).with_context(|| {
        format!(
            "parsing the shim request as JSON ({} bytes read)",
            line.len()
        )
    })
}

/// Apply the kernel-level lockdown described by `req`.
///
/// Fails closed: any error here must abort before the command runs, since the
/// alternative is executing untrusted code with less confinement than intended.
fn apply_lockdown(req: &ShimRequest) -> Result<()> {
    super::lockdown::apply(req)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request crosses a process boundary as JSON, so its shape is a contract.
    #[test]
    fn request_round_trips() {
        let req = ShimRequest {
            command: "echo hi".into(),
            read_only: vec!["/usr".into()],
            read_write: vec!["/workspace".into()],
            list_dirs: vec!["/".into()],
            scope_ipc: true,
            deny_syscalls: vec!["io_uring_setup".into()],
            deny_raw_sockets: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<ShimRequest>(&json).unwrap(), req);
    }

    /// Missing optional fields must default rather than fail, so an older shim
    /// binary meeting a newer request does not abort a session.
    #[test]
    fn minimal_request_parses() {
        let req: ShimRequest = serde_json::from_str(r#"{"command":"true"}"#).unwrap();
        assert_eq!(req.command, "true");
        assert!(req.read_only.is_empty());
        assert!(!req.deny_raw_sockets);
    }
}
