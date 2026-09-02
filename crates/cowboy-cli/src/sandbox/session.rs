//! The session sandbox: namespaces that outlive a single command.
//!
//! Two lifetimes, split on purpose:
//!
//! | Namespace        | Lifetime    | Why |
//! |------------------|-------------|-----|
//! | user, net, ipc, uts | **session** | A dev server started by one command must be reachable from the next, which needs a shared network namespace. |
//! | mount            | per command | Rebuilt from the current grant set, so a path approved a moment ago is simply in the next command's bind list. |
//! | pid             | per command | Killing bwrap then reaps exactly that command's processes and nothing else. |
//!
//! Note that commands deliberately do **not** share a PID namespace. Sharing it
//! would break the clean per-command reap (`--die-with-parent` only cascades when
//! bwrap is the namespace's own PID 1) and would let one command see and signal
//! another's processes. Verified: two commands in one session reach the same
//! loopback service while each sees only its own PIDs.
//!
//! The namespaces are held open by a small holder process. Per command, the host
//! `setns()`es into that holder's user and network namespaces **in the forked child
//! via `pre_exec`**, before `exec`ing bwrap — never on a thread of the main process,
//! which would move the whole worker into the sandbox's namespaces.

use std::ffi::CString;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{bail, Context, Result};

/// Printed by the holder once its namespaces exist and loopback is up.
const READY: &str = "cowboy-sandbox-ready";

/// Namespaces shared by every command in one session.
pub struct SessionSandbox {
    holder: Child,
    /// The holder's pid, and so the path to its namespace files.
    pid: u32,
    name: String,
}

impl SessionSandbox {
    /// Create the session's namespaces.
    ///
    /// `holder_exe` must be the `cowboy` binary. It is passed in rather than taken
    /// from `current_exe()` because those differ whenever cowboy is not the running
    /// program — in an integration test, `current_exe()` is the test harness, which
    /// would be launched with an argument it does not understand and fail with no
    /// useful message. It also keeps this agreeing with the binary the plan binds as
    /// the lockdown shim.
    pub fn start(name: &str, holder_exe: &Path) -> Result<Self> {
        let mut cmd = Command::new("unshare");
        cmd.args(["--user", "--map-root-user", "--net", "--ipc", "--uts", "--"]);
        cmd.arg(holder_exe).arg("x-sandbox-holder");
        // stdin is the liveness channel: the holder blocks reading it, so if this
        // process dies the read returns EOF and the holder exits, taking the
        // namespaces with it. No orphaned session namespaces after a crash.
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut holder = cmd.spawn().context(
            "starting the sandbox session holder (is util-linux `unshare` installed, and are \
             unprivileged user namespaces enabled?)",
        )?;
        let pid = holder.id();

        // Wait for readiness before anyone tries to join: the namespaces do not
        // exist until `unshare` has done its work, and joining too early would fail
        // with a confusing ENOENT on /proc/<pid>/ns/net.
        let stdout = holder.stdout.take().context("holder stdout")?;
        let mut lines = BufReader::new(stdout).lines();
        let mut first = None;
        for line in lines.by_ref() {
            match line {
                // Tolerate blank lines so a stray newline is not read as failure.
                Ok(l) if l.trim().is_empty() => continue,
                Ok(l) => {
                    first = Some(l);
                    break;
                }
                Err(_) => break,
            }
        }
        if first.as_deref().map(str::trim) != Some(READY) {
            let mut err = String::new();
            if let Some(mut e) = holder.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            let _ = holder.kill();
            let _ = holder.wait();
            bail!(
                "the sandbox session holder did not come up (it said {first:?}). {}",
                if err.trim().is_empty() {
                    format!(
                        "No error output. Check that {} is the cowboy binary.",
                        holder_exe.display()
                    )
                } else {
                    format!("Its error output: {}", err.trim())
                }
            );
        }

        Ok(Self {
            holder,
            pid,
            name: name.to_string(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn ns_path(&self, kind: &str) -> PathBuf {
        PathBuf::from(format!("/proc/{}/ns/{kind}", self.pid))
    }

    /// Whether the holder is still alive, so a caller can fail closed rather than
    /// run a command in namespaces that no longer exist.
    pub fn is_alive(&self) -> bool {
        self.ns_path("net").exists()
    }

    /// Arrange for `cmd`'s child to run inside this session's namespaces.
    ///
    /// The `setns` calls happen in the forked child via `pre_exec`, so the calling
    /// thread is unaffected — doing it inline would move the worker process itself
    /// into the sandbox's network namespace and cut it off from the model provider.
    ///
    /// The user namespace must be joined **first**: joining only the network
    /// namespace fails with `EPERM`, because the privilege to do so comes from
    /// membership of the user namespace that owns it.
    pub fn enter(&self, cmd: &mut Command) -> Result<()> {
        // Paths are turned into CStrings up front: `pre_exec` runs between fork and
        // exec, where allocation is best avoided.
        let user = cstring(self.ns_path("user"))?;
        let net = cstring(self.ns_path("net"))?;
        let ipc = cstring(self.ns_path("ipc"))?;
        let uts = cstring(self.ns_path("uts"))?;

        // SAFETY: `pre_exec` requires async-signal-safe work only. open/setns/close
        // are raw syscalls with no locking and no allocation.
        unsafe {
            cmd.pre_exec(move || {
                join(&user, libc::CLONE_NEWUSER)?;
                join(&net, libc::CLONE_NEWNET)?;
                join(&ipc, libc::CLONE_NEWIPC)?;
                join(&uts, libc::CLONE_NEWUTS)?;
                Ok(())
            });
        }
        Ok(())
    }

    /// Tear the session down, releasing its namespaces.
    ///
    /// Anything still running inside loses its network namespace, which is the point:
    /// no background process outlives the session that owns it.
    pub fn stop(&mut self) {
        // Closing stdin is the cooperative signal; the kill is the backstop.
        drop(self.holder.stdin.take());
        let _ = self.holder.kill();
        let _ = self.holder.wait();
    }
}

impl Drop for SessionSandbox {
    fn drop(&mut self) {
        self.stop();
    }
}

fn cstring(p: PathBuf) -> Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(p.as_os_str().as_bytes())
        .with_context(|| format!("namespace path {} contains a NUL", p.display()))
}

/// `setns` on a namespace file. Runs in the pre-exec child, so it returns
/// `io::Error` rather than using anyhow.
fn join(path: &CString, kind: i32) -> std::io::Result<()> {
    // SAFETY: `path` is a valid NUL-terminated string; the fd is closed below.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a namespace file descriptor we just opened.
    let rc = unsafe { libc::setns(fd, kind) };
    // SAFETY: closing our own fd.
    unsafe { libc::close(fd) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `cowboy x-sandbox-holder` — keeps the session's namespaces alive.
///
/// Runs *inside* the new namespaces (started via `unshare`). It brings loopback up,
/// signals readiness, then blocks on stdin so that closing it — or the parent dying
/// — ends the session.
pub fn run_holder() -> Result<()> {
    // Loopback is DOWN in a fresh network namespace, so without this every
    // connection to 127.0.0.1 inside the sandbox fails. The holder can do it
    // because it holds CAP_NET_ADMIN in its own user namespace; no agent process
    // ever does.
    let out = Command::new("ip")
        .args(["link", "set", "lo", "up"])
        .output()
        .context("running `ip link set lo up` (is iproute2 installed?)")?;
    if !out.status.success() {
        bail!(
            "could not bring loopback up in the session network namespace: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    println!("{READY}");
    use std::io::Write;
    std::io::stdout().flush().ok();

    // Block until stdin closes. Reading (rather than sleeping forever) is what ties
    // the session's lifetime to its parent's.
    let mut buf = [0u8; 1];
    loop {
        match std::io::stdin().read(&mut buf) {
            Ok(0) => return Ok(()), // parent gone
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_paths_follow_the_holder_pid() {
        // Constructed directly rather than started, so this stays a pure test.
        let s = SessionSandbox {
            holder: Command::new("true").spawn().unwrap(),
            pid: 4242,
            name: "t".into(),
        };
        assert_eq!(s.ns_path("net"), PathBuf::from("/proc/4242/ns/net"));
        assert_eq!(s.ns_path("user"), PathBuf::from("/proc/4242/ns/user"));
        std::mem::forget(s); // don't run Drop's kill on an unrelated pid
    }

    #[test]
    fn cstring_rejects_an_embedded_nul() {
        assert!(cstring(PathBuf::from("a\0b")).is_err());
    }
}
